#!/bin/bash
# E2E migration validation script using QEMU
# Run this script as root or with sudo access to allow loop device mounts.

set -euo pipefail

# Unique LUKS mapper name per run to avoid stale dm device conflicts
UUID_SUFFIX=$(date +%s)_$$  # timestamp + PID should be unique enough
LUKS_MAPPER="e2e-root_${UUID_SUFFIX}"
# Passphrase for the LUKS root in the xfs+crypt scenario; injected over the
# serial console at the boot unlock prompt.
LUKS_PASSPHRASE="testpassphrase"

# Configurable parameters
BASE_IMAGE="${BASE_IMAGE:-ghcr.io/projectbluefin/bluefin:stable}"
TARGET_IMAGE="${TARGET_IMAGE:-ghcr.io/projectbluefin/dakota:stable}"
DISK_SIZE="${DISK_SIZE:-20G}"
SSH_PORT="${SSH_PORT:-2222}"
# Derived from TARGET_IMAGE: extracts the repo:tag part (e.g. "dakota:stable"
# from "ghcr.io/projectbluefin/dakota:stable"). Used by the subscription check.
TARGET_REPO_TAG="${TARGET_REPO_TAG:-$(echo "$TARGET_IMAGE" | sed 's|.*/||')}"
FILESYSTEM="${FILESYSTEM:-btrfs}"
# What this run exercises. "composefs-migrate" is the full migrator pipeline
# (the default, unchanged). "ostree-rebase-plan" is the tracer for the
# ostree→ostree re-base (#63): boot the source VM, assert bootc-rebase
# resolves the OstreeDeploy route on a real OSTree system, and exit —
# upgraded to the full re-base + validation when Strategy::OstreeDeploy
# lands (#64). "tui-migrate" is the composefs pipeline with the human at
# the controls simulated: the interactive Config Drift Review checklist
# (#15) and then the whole migration are driven through the TUI wizard by
# tests/tui-e2e-driver.py on the VM, and every downstream assertion of
# the default mode runs unchanged — this is the cell that proves the
# terminal event loops CI could previously only compile (ROADMAP M5).
E2E_MODE="${E2E_MODE:-composefs-migrate}"
# Scenario capability flags derived from FILESYSTEM. Both encrypted scenarios
# share all the LUKS plumbing (swtpm, serial passphrase injection, BLS karg
# patching, skipping host-side SSH injection); the lvm variant additionally
# carves a VG with separate root + /var logical volumes inside the LUKS
# container, reproducing a dedicated-/var layout (e.g. anaconda's default).
IS_LUKS=false
IS_LVM=false
case "$FILESYSTEM" in
    xfs+crypt) IS_LUKS=true ;;
    xfs+lvm+crypt) IS_LUKS=true; IS_LVM=true ;;
esac
# Volume group + logical volume names for the LVM scenario.
LVM_VG="e2e_vg_${UUID_SUFFIX}"
# Test variant: "migrate" (default — migrate, commit, rollback round-trip) or
# "undo" (migrate, then verify `undo` cleans up and falls back to OSTree).
E2E_TEST_MODE="${E2E_TEST_MODE:-migrate}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

step() { printf '\033[1;36m[e2e %s]\033[0m %s\n' "$(date +%H:%M:%S)" "$*"; }

# CI-friendly serial-console filter. The raw QEMU log dumps thousands of systemd
# `[ OK ] Started …` lines per boot — readable on a TTY, useless in CI. Only
# forward lines that carry actual signal (failures, sshd/login activity, BLS
# entries, kernel panics, our migration markers). The full unfiltered log
# remains on disk at qemu.log for forensic inspection.
vm_tail() {
    local prefix="${1:-vm-serial}"
    tail -F -q -n 0 qemu.log 2>/dev/null \
      | sed -u 's/\x1b\[[0-9;]*[a-zA-Z]//g; s/\x1b[()][0-9A-Za-z]//g' \
      | grep --line-buffered -E '\[FAILED\]|Failed to start|panic|Out of memory|kernel BUG|Kernel panic|sshd|fedora login:|Welcome to|GRUB|Booting|systemd-boot|composefs|=== Phase|=== MIGRATION|bootc-migrate|Bluefin \(Version|Dakota|Linux Boot Manager|dbus|messagebus|polkit|logind|machine.id|failed with|error.*starting' \
      | awk -v p="$prefix" '{ print "[" p "] " $0; fflush() }'
}

# heartbeat: while $1 is a live PID, prints a "[e2e HH:MM:SS] still <label>
# (Ns elapsed)" line every $2 seconds so CI doesn't think the job is hung.
heartbeat() {
    local pid="$1" interval="${2:-15}" label="${3:-working}"
    local started=$SECONDS
    while kill -0 "$pid" 2>/dev/null; do
        sleep "$interval"
        kill -0 "$pid" 2>/dev/null || break
        printf '\033[2;36m[e2e %s]\033[0m still %s (%ds elapsed)\n' \
            "$(date +%H:%M:%S)" "$label" "$((SECONDS - started))"
    done
}

# Kill any stray QEMU/tail processes from a previous interrupted run so this
# invocation isn't competing for SSH_PORT or polluting qemu.log with stale tails.
# Identify victims via pgrep into a variable so the kill command line doesn't
# contain a pattern that would match (and SIGKILL) its own sudo wrapper.
step "Reaping stray processes from prior runs..."
{
    qemu_pids=$(sudo ss -lntpH "sport = :${SSH_PORT}" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u || true)
    tail_pids=$(pgrep -f 'tail -F.* qemu.log$' || true)
    reap_pids=$(printf '%s\n%s\n' "$qemu_pids" "$tail_pids" | grep -v '^$' | sort -u || true)
    if [ -n "$reap_pids" ]; then
        echo "Reaping PIDs: $reap_pids"
        # shellcheck disable=SC2086
        sudo kill -9 $reap_pids 2>/dev/null || true
    fi
} || true
# Nuke stale LUKS dm mapper from prior runs (kernel holds dm devices open even after loop cleanup).
if [ -L /dev/mapper/"$LUKS_MAPPER" ]; then
    sudo dmsetup remove -f "$LUKS_MAPPER" 2>/dev/null || \
        (sudo dmsetup message "$LUKS_MAPPER" 0 "key wipe" 2>/dev/null; sleep 1; sudo dmsetup remove -f "$LUKS_MAPPER" 2>/dev/null) || true
fi
# Wait until the port is free so QEMU can bind it.
for _ in $(seq 1 10); do
    if ! sudo ss -lntH "sport = :${SSH_PORT}" 2>/dev/null | grep -q .; then break; fi
    sleep 1
done

# Cleanup artifacts from previous runs
rm -f "$WORKSPACE_DIR"/disk.raw "$WORKSPACE_DIR"/qemu.log "$WORKSPACE_DIR"/test_key "$WORKSPACE_DIR"/test_key.pub

step "=== E2E Test Configuration ==="
echo "Base Image (OSTree):   $BASE_IMAGE"
echo "Target Image (Cfs):    $TARGET_IMAGE"
echo "Disk Size:             $DISK_SIZE"
echo "Workspace:             $WORKSPACE_DIR"

# 1. Preflight checks
step "=== Preflight checks ==="
if ! command -v qemu-system-x86_64 &>/dev/null; then
    echo "ERROR: qemu-system-x86_64 not found."
    exit 1
fi
if ! command -v podman &>/dev/null; then
    echo "ERROR: podman not found."
    exit 1
fi

# Locate a matched OVMF CODE + VARS pair. Mixing CODE from one build with VARS
# from another (e.g. brew-packaged edk2-x86_64-code.fd + Fedora OVMF_VARS_4M.fd)
# yields a flash image OVMF treats as uninitialised — NVRAM writes succeed via
# efivarfs but never make it to the VARS file, so BootOrder changes don't
# survive reboot. Prefer host-installed paired sets; otherwise pull the
# OVMF_CODE_4M.fd / OVMF_VARS_4M.fd pair out of a container storage layer.
OVMF_PATH=""
OVMF_VARS_TEMPLATE=""
declare -a OVMF_PAIRS=(
    "/usr/share/OVMF/OVMF_CODE_4M.fd:/usr/share/OVMF/OVMF_VARS_4M.fd"
    "/usr/share/OVMF/OVMF_CODE.fd:/usr/share/OVMF/OVMF_VARS.fd"
    "/usr/share/edk2/ovmf/OVMF_CODE.fd:/usr/share/edk2/ovmf/OVMF_VARS.fd"
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd:/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"
)
for pair in "${OVMF_PAIRS[@]}"; do
    code="${pair%%:*}"; vars="${pair##*:}"
    if [ -f "$code" ] && [ -f "$vars" ]; then
        OVMF_PATH="$code"; OVMF_VARS_TEMPLATE="$vars"; break
    fi
done
if [ -z "$OVMF_PATH" ]; then
    # Last-resort: find a matched pair under container storage layers.
    found=$(sudo find /sysroot /var/lib/containers -type d -path '*share/OVMF' 2>/dev/null | head -1)
    if [ -n "$found" ] && [ -f "$found/OVMF_CODE_4M.fd" ] && [ -f "$found/OVMF_VARS_4M.fd" ]; then
        OVMF_PATH="$found/OVMF_CODE_4M.fd"
        OVMF_VARS_TEMPLATE="$found/OVMF_VARS_4M.fd"
    fi
fi
if [ -z "$OVMF_PATH" ] || [ -z "$OVMF_VARS_TEMPLATE" ]; then
    echo "ERROR: no matched OVMF CODE+VARS pair found." >&2
    echo "       NVRAM persistence is required for the systemd-boot post-reboot" >&2
    echo "       check; install OVMF or extract OVMF_CODE_4M.fd + OVMF_VARS_4M.fd" >&2
    echo "       from a Fedora container image." >&2
    exit 1
fi
echo "Using UEFI firmware:   $OVMF_PATH"
echo "Using UEFI VARS:       $OVMF_VARS_TEMPLATE"

# 2. Build the migration binary
step "=== Building migration utility ==="
cd "$WORKSPACE_DIR"
cargo build

# 3. Create SSH key for E2E automation
step "=== Generating SSH test key ==="
rm -f ./test_key ./test_key.pub
ssh-keygen -t rsa -N "" -f ./test_key

# 4. Prepare bootable image with sshd enabled (some OSTree images like Bluefin disable sshd by default)
step "=== Preparing bootable image with sshd ==="
MODIFIED_IMAGE="localhost/e2e-bluefin-ssh:latest"

# Create a Containerfile that enables sshd
TMP_CONTAINERFILE=$(mktemp)
cat > "$TMP_CONTAINERFILE" <<'DOCKERFILE'
FROM BASE_IMAGE_PLACEHOLDER
# Preset (covers fresh boots) AND direct symlink (covers images where sshd
# was already preset-disabled at build time — the preset file alone wouldn't
# re-enable it). systemctl enable in a build container only writes symlinks,
# which is exactly what we want.
RUN mkdir -p /usr/lib/systemd/system-preset && \
    echo 'enable sshd.service' > /usr/lib/systemd/system-preset/50-e2e-ssh.preset && \
    echo 'enable sshd.socket' >> /usr/lib/systemd/system-preset/50-e2e-ssh.preset
RUN systemctl enable sshd.service && systemctl enable sshd.socket || true
# Direct symlink fallback in case systemctl enable was a no-op
RUN mkdir -p /usr/lib/systemd/system/multi-user.target.wants && \
    ln -sf /usr/lib/systemd/system/sshd.service \
           /usr/lib/systemd/system/multi-user.target.wants/sshd.service
# NOTE: e2e-sshd.socket, PermitRootLogin, and other user-specific /etc
# customizations are baked into the base image here for Bluefin's first boot.
# The ComposeFS 3-way merge will drop these (old==cur, new absent), but
# Phase 4's `ensure_e2e_ssh_socket` recreates them in the deploy /etc so
# they're present on the Dakota composefs boot too.
RUN echo 'PermitRootLogin yes' >> /etc/ssh/sshd_config && \
    echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config && \
    echo 'Port 22' >> /etc/ssh/sshd_config
# Disable firewalld (CentOS Stream 10 blocks SSH by default)
RUN systemctl disable firewalld 2>/dev/null || true
# e2e-sshd.socket for TCP 22 (Bluefin's sshd only binds Unix-local + vsock).
RUN mkdir -p /etc/systemd/system && \
    printf '%s\n' \
        '[Unit]' \
        'Description=E2E SSH TCP Socket (port 22)' \
        '[Socket]' \
        'ListenStream=22' \
        'Accept=yes' \
        '[Install]' \
        'WantedBy=sockets.target' \
        > /etc/systemd/system/e2e-sshd.socket && \
    printf '%s\n' \
        '[Unit]' \
        'Description=E2E SSH per-connection service' \
        '[Service]' \
        'ExecStart=-/usr/sbin/sshd -i' \
        'StandardInput=socket' \
        > /etc/systemd/system/e2e-sshd@.service && \
    mkdir -p /etc/systemd/system/sockets.target.wants && \
    ln -sf /etc/systemd/system/e2e-sshd.socket \
           /etc/systemd/system/sockets.target.wants/e2e-sshd.socket
DOCKERFILE

# Substitute the base image
sed -i "s|BASE_IMAGE_PLACEHOLDER|$BASE_IMAGE|g" "$TMP_CONTAINERFILE"

echo "Building modified base image with sshd enabled..."
if ! sudo podman image exists "$BASE_IMAGE" 2>/dev/null; then
    echo "Pulling base image $BASE_IMAGE..."
    sudo podman pull "$BASE_IMAGE"
fi
sudo podman build -t "$MODIFIED_IMAGE" -f "$TMP_CONTAINERFILE"
rm -f "$TMP_CONTAINERFILE"

INSTALL_IMAGE="$MODIFIED_IMAGE"
echo "Using install image: $INSTALL_IMAGE"

# NOTE: TARGET_IMAGE stays the pristine upstream ref (see the "pulled directly
# by the migration" comment near VM_TARGET_IMAGE below) — the migration/rebase
# binaries pull it live, from inside the VM, over the network. A locally-built
# "localhost/..." target image is unreachable from inside the VM (no registry
# mirror; that was tried and rejected for cost — see that comment) and
# harmless deviation on the target image is out of scope for the E2E's sshd
# setup: post-merge injection (below, mirroring the ostree-rebase path) covers
# it without touching what the migration actually pulls.

# 5. Create and initialize disk image (or restore checkpoint)
# LUKS: checkpoint has plain partition layout, not LUKS — always full setup.
CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"
if [ "$IS_LUKS" = true ]; then
    echo "LUKS mode: skipping pre-migration checkpoint (needs fresh LUKS setup)"
    SKIP_SETUP=false
elif [ -f "$CHECKPOINT" ]; then
    step "=== Restoring pre-migration checkpoint ==="
    cp "$CHECKPOINT" disk.raw
    SKIP_SETUP=true

    # test_key is regenerated each run, so the checkpoint's stale authorized_keys
    # would lock us out. Reseed it with the fresh pubkey before booting.
    step "=== Reseeding authorized_keys in checkpoint ==="
    CKPT_LOOP=$(sudo losetup --show -f -P disk.raw)
    # Find the root partition (p2 is the ESP on bootc-installed disks).
    CKPT_ROOT=""
    # udev / partition scanning can be asynchronous; retry with partprobe + settle
    for i in $(seq 1 10); do
        sudo partprobe "$CKPT_LOOP" 2>/dev/null || true
        sudo udevadm settle 2>/dev/null || true
        for p in "${CKPT_LOOP}"p*; do
            if sudo blkid -o value -s TYPE "$p" 2>/dev/null | grep -qx "$FILESYSTEM"; then
                CKPT_ROOT="$p"; break 2
            fi
        done
        sleep 1
    done
    if [ -z "$CKPT_ROOT" ]; then
        echo "ERROR: could not find $FILESYSTEM root partition on $CKPT_LOOP" >&2
        sudo losetup -d "$CKPT_LOOP"; exit 1
    fi
    CKPT_MNT="/tmp/mnt-e2e-ckpt"
    sudo mkdir -p "$CKPT_MNT"
    sudo mount "$CKPT_ROOT" "$CKPT_MNT"
    CKPT_SSH="$CKPT_MNT/ostree/deploy/default/var/roothome/.ssh"
    sudo mkdir -p "$CKPT_SSH"
    sudo chmod 700 "$CKPT_SSH"
    sudo cp ./test_key.pub "$CKPT_SSH/authorized_keys"
    sudo chmod 600 "$CKPT_SSH/authorized_keys"
    sudo umount "$CKPT_MNT"
    sudo losetup -d "$CKPT_LOOP"
else
    SKIP_SETUP=false
fi

# Install cleanup trap before any long-lived child processes are spawned, so
# QEMU and serial-tail processes are reaped even when restoring from checkpoint
# or if the script is interrupted (SIGINT/SIGTERM/EXIT).
cleanup() {
    if [ -n "${TAIL_PID:-}" ]; then
        kill "$TAIL_PID" 2>/dev/null || true
    fi
    if [ -n "${HB_PID:-}" ]; then
        kill "$HB_PID" 2>/dev/null || true
    fi
    # Kill any orphaned tail processes to prevent hanging stdout/stderr in CI
    sudo pkill -f 'tail -F.*qemu.log' 2>/dev/null || true
    if [ -n "${QEMU_PID:-}" ]; then
        step "Terminating QEMU (PID: $QEMU_PID)..."
        sudo kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    if [ -n "${LOOP_DEV:-}" ]; then
        step "Detaching loopback device $LOOP_DEV..."
        sudo umount /tmp/mnt-e2e-disk 2>/dev/null || true
        # Deactivate the LVM VG (if any) before closing the LUKS container that
        # holds its PV, otherwise cryptsetup close fails with "device in use".
        if [ "${IS_LVM:-false}" = true ]; then
            sudo vgchange -an "$LVM_VG" 2>/dev/null || true
        fi
        sudo cryptsetup close "$LUKS_MAPPER" 2>/dev/null || true
        sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
    # Clean up swtpm if running
    if [ -n "${SWTPM_PID:-}" ]; then
        kill "$SWTPM_PID" 2>/dev/null || true
        rm -rf /tmp/swtpm-tpmstate /tmp/swtpm-sock 2>/dev/null || true
    fi
    # Clean up the LUKS passphrase injector + its FIFO.
    if [ -n "${PW_INJECTOR_PID:-}" ]; then
        kill "$PW_INJECTOR_PID" 2>/dev/null || true
    fi
    exec 9>&- 2>/dev/null || true
    rm -f /tmp/e2e-qemu-stdin 2>/dev/null || true
    rm -f ./test_key ./test_key.pub
}
trap cleanup EXIT

if [ "$SKIP_SETUP" = false ]; then
step "=== Creating disk image ==="
rm -f disk.raw
truncate -s "$DISK_SIZE" disk.raw

echo "Setting up loopback device..."
LOOP_DEV=$(sudo losetup --show -f -P disk.raw)
echo "Mounted loopback device: $LOOP_DEV"

if [ "$IS_LUKS" = true ]; then
    # Install the GRUB-based source (Bluefin) with a LUKS-encrypted root, then
    # let the migration convert it to systemd-boot + composefs. We drive LUKS
    # ourselves and use `bootc install to-filesystem` (fisherman's process)
    # rather than `--block-setup tpm2-luks`, because the latter seals the key to
    # whatever TPM exists during the container install — not the VM's vTPM.
    #
    # Layout mirrors fisherman's DiskLayoutGrub: ESP + a separate *unencrypted*
    # ext4 /boot (GRUB reads kernel/initrd here without parsing the LUKS/xfs
    # root) + LUKS2 root. Unlock uses a keyfile on /boot for a deterministic,
    # non-interactive boot (production enrolls TPM2/PCR7 instead — see
    # docs/luks-testing.md).
    #
    # In the lvm variant the LUKS container holds an LVM PV/VG with *separate*
    # root and /var logical volumes — reproducing a dedicated-/var layout
    # (anaconda's default, e.g. kanpur). This is the case the migrator's
    # rd.lvm.lv discovery must handle: the source GRUB entry activates only the
    # root LV (rd.lvm.lv=<vg>/root) and relies on post-switchroot auto-activation
    # for /var, so /proc/cmdline at migration time never mentions the var LV.
    echo "LUKS: partitioning (BIOS-boot + ESP + ext4 /boot + LUKS root)..."
    # GPT layout matching what `bootc install to-disk --generic-image` creates:
    # a 1 MiB BIOS boot partition (type 21686148-... = bios_grub) so grub2-install
    # can embed its i386-pc core (otherwise it fails with "will not proceed with
    # blocklists"), an ESP, a separate ext4 /boot for GRUB, then the LUKS root.
    sudo sfdisk "$LOOP_DEV" <<'SFDISK'
label: gpt
size=1MiB,   type=21686148-6449-6E6F-744E-656564454649, name="BIOS-BOOT"
size=512MiB, type=uefi, name="EFI-SYSTEM"
size=1GiB,   type=linux, name="boot"
type=linux, name="root"
SFDISK
    sudo partprobe "$LOOP_DEV" 2>/dev/null || true
    sudo udevadm settle 2>/dev/null || true
    ESP_PART="${LOOP_DEV}p2"
    BOOT_PART="${LOOP_DEV}p3"
    ROOT_PART="${LOOP_DEV}p4"

    echo "LUKS: formatting root as LUKS2 with a passphrase..."
    # el10's systemd-cryptsetup ignores dracut's rd.luks.key=path:dev syntax, so
    # a keyfile on /boot doesn't auto-unlock. Instead use a known passphrase and
    # inject it over the serial console at the boot prompt (see the QEMU launch).
    printf '%s' "$LUKS_PASSPHRASE" | sudo cryptsetup luksFormat --batch-mode \
        --type luks2 --key-file - "$ROOT_PART"
    printf '%s' "$LUKS_PASSPHRASE" | sudo cryptsetup luksOpen \
        --key-file - "$ROOT_PART" "$LUKS_MAPPER"
    LUKS_UUID=$(sudo cryptsetup luksUUID "$ROOT_PART")

    # Root and var device paths. Plain LUKS uses the mapper directly; the lvm
    # variant carves a VG with separate root + var LVs inside the LUKS container.
    ROOT_DEV="/dev/mapper/$LUKS_MAPPER"
    VAR_DEV=""
    lvm_root_arg=""
    if [ "$IS_LVM" = true ]; then
        echo "LVM: creating PV/VG ($LVM_VG) with separate root + var LVs inside LUKS..."
        sudo pvcreate -ff -y "/dev/mapper/$LUKS_MAPPER"
        sudo vgcreate "$LVM_VG" "/dev/mapper/$LUKS_MAPPER"
        # /var gets 20G, not the fixed 4G it had until 2026-08-27: it holds
        # far more than "small test fixtures", because the guest's podman
        # storage (/var/lib/containers/storage) is where Phase 2 pulls the
        # ~5 GB-compressed target image. At 4G that pull dies with
        # "no space left on device" partway through — which is why the two
        # earlier DISK_SIZE bumps (20G -> 40G -> 60G, #42) never helped:
        # a bigger disk only grows *root*, never this fixed-size LV. The
        # cell was gated off (KVM_E2E_ENABLED unset) for the whole period,
        # so nothing ever executed the 60G config until the matrix moved to
        # hosted runners. Root still takes the rest (~38G of a 60G disk),
        # which is what the passing single-filesystem composefs cells get
        # for the whole job. Separate volumes remains the point: /var lives
        # on its own LV.
        sudo lvcreate -y -L 20G -n var "$LVM_VG"
        sudo lvcreate -y -l 100%FREE -n root "$LVM_VG"
        sudo vgchange -ay "$LVM_VG"
        sudo udevadm settle 2>/dev/null || true
        ROOT_DEV="/dev/$LVM_VG/root"
        VAR_DEV="/dev/$LVM_VG/var"
        # Activate only root at boot via rd.lvm.lv — the var LV is intentionally
        # left to auto-activation so it never appears on the source cmdline,
        # faithfully reproducing the bug the migrator must fix.
        lvm_root_arg="rd.lvm.lv=$LVM_VG/root"
    fi

    echo "LUKS: making filesystems and mounting target..."
    # The host may lack a usable system mkfs.xfs (e.g. only a HOME-relative user
    # build that breaks under sudo). The install image ships real
    # xfsprogs/e2fsprogs/dosfstools, so format the partitions inside it;
    # --privileged + /dev exposes the loop partitions and the opened LUKS mapper.
    sudo podman run --privileged --rm -v /dev:/dev "$INSTALL_IMAGE" bash -c "
        set -e
        mkfs.xfs -f $ROOT_DEV
        ${VAR_DEV:+mkfs.xfs -f $VAR_DEV}
        mkfs.ext4 -F $BOOT_PART
        mkfs.vfat -F32 $ESP_PART"
    # Use /var/tmp (not /tmp, which is tmpfs on many hosts) as the mount point.
    INSTALL_ROOT=/var/tmp/mnt-e2e-install
    sudo mkdir -p "$INSTALL_ROOT"
    sudo mount "$ROOT_DEV" "$INSTALL_ROOT"
    if [ "$IS_LVM" = true ]; then
        # Mount the dedicated /var LV under the target so bootc populates it and
        # records it; an explicit fstab entry is added after install for certainty.
        sudo mkdir -p "$INSTALL_ROOT/var"
        sudo mount "$VAR_DEV" "$INSTALL_ROOT/var"
    fi
    sudo mkdir -p "$INSTALL_ROOT/boot"
    sudo mount "$BOOT_PART" "$INSTALL_ROOT/boot"
    sudo mkdir -p "$INSTALL_ROOT/boot/efi"
    sudo mount "$ESP_PART" "$INSTALL_ROOT/boot/efi"
    df -h "$INSTALL_ROOT" "$INSTALL_ROOT/boot" "$INSTALL_ROOT/boot/efi"

    echo "LUKS: installing source image into the opened target via to-filesystem..."
    # Mirrors fisherman's container invocation: bind-propagation=rslave makes the
    # nested /boot and /boot/efi mounts visible inside the container (without it
    # bootc writes to the wrong filesystem and trips ostree min-free-space);
    # label=disable lets bootc write security.selinux xattrs to the target.
    # Bind a real, disk-backed /var/tmp: ostree/containers-storage writes
    # multi-GB intermediate import blobs there, and the container's default
    # /var/tmp (overlay/tmpfs) is too small — it trips ostree's
    # min-free-space-percent guard (dakota-iso's LUKS ENOSPC lesson).
    sudo podman run --privileged --pid=host --rm \
        --security-opt label=disable \
        -v /dev:/dev \
        -v /sys:/sys \
        -v /var/tmp:/var/tmp \
        -v "$WORKSPACE_DIR":/workspace \
        --mount "type=bind,src=$INSTALL_ROOT,dst=/target,bind-propagation=rslave" \
        "$INSTALL_IMAGE" \
        bootc install to-filesystem \
        --generic-image \
        --skip-fetch-check \
        --root-ssh-authorized-keys /workspace/test_key.pub \
        /target

    echo "LUKS: writing LUKS kernel args to GRUB BLS entries..."
    # bootc remounts the target filesystems read-only when finalizing the
    # install; remount /boot rw so we can patch the BLS entries.
    sudo mount -o remount,rw "$INSTALL_ROOT/boot" 2>/dev/null || true
    sudo mount -o remount,rw "$INSTALL_ROOT" 2>/dev/null || true
    # rd.luks.name=<UUID>=root maps the container to /dev/mapper/root, which
    # systemd-gpt-auto-generator needs to locate the encrypted root (the bare
    # mapper-name form silently fails — projectbluefin/dakota#270). For the lvm
    # variant we also activate the root LV (rd.lvm.lv=<vg>/root) — but NOT the
    # var LV, mirroring the real-world source layout.
    luks_args="rd.luks.name=$LUKS_UUID=root${lvm_root_arg:+ $lvm_root_arg}"
    for bls in "$INSTALL_ROOT"/boot/loader/entries/*.conf; do
        [ -f "$bls" ] || continue
        if ! grep -q 'rd.luks' "$bls"; then
            sudo sed -i "s|^\(options .*\)|\1 $luks_args|" "$bls"
            echo "  patched $(basename "$bls")"
        fi
    done

    if [ "$IS_LVM" = true ]; then
        echo "LVM: ensuring a /var fstab entry for the dedicated var LV..."
        # The source (and the migrated system) must mount /var from the var LV.
        # Add a UUID-based entry to the deployment's /etc/fstab if bootc didn't.
        VAR_UUID=$(sudo blkid -o value -s UUID "$VAR_DEV")
        for fstab in "$INSTALL_ROOT"/ostree/deploy/*/deploy/*/etc/fstab; do
            [ -f "$fstab" ] || continue
            if ! grep -qE '[[:space:]]/var[[:space:]]' "$fstab"; then
                echo "UUID=$VAR_UUID /var xfs defaults 0 0" | sudo tee -a "$fstab" >/dev/null
                echo "  added /var entry (UUID=$VAR_UUID) to $(basename "$(dirname "$fstab")")/etc/fstab"
            fi
        done
    fi

    echo "LUKS: unmounting and closing..."
    sudo umount "$INSTALL_ROOT/boot/efi" "$INSTALL_ROOT/boot" 2>/dev/null || true
    [ "$IS_LVM" = true ] && sudo umount "$INSTALL_ROOT/var" 2>/dev/null || true
    sudo umount "$INSTALL_ROOT" 2>/dev/null || true
    if [ "$IS_LVM" = true ]; then
        sudo vgchange -an "$LVM_VG" 2>/dev/null || true
    fi
    sudo cryptsetup luksClose "$LUKS_MAPPER" 2>/dev/null || true
    SKIP_SETUP=true
    echo "LUKS disk setup complete (bootc install to-filesystem + keyfile, GRUB source)"
else
    echo "Installing base OSTree bootc system to disk image..."
    # Run bootc install to-disk using podman on the loop device
    sudo podman run --privileged --pid=host --rm \
        -v /dev:/dev \
        -v /var/tmp:/var/tmp \
        -v /tmp:/tmp \
        -v "$WORKSPACE_DIR":/workspace \
        "$INSTALL_IMAGE" \
        bootc install to-disk \
        --generic-image \
        --filesystem "$FILESYSTEM" \
        --root-ssh-authorized-keys /workspace/test_key.pub \
        "$LOOP_DEV"
fi
fi

# Force kernel to reread partition table by detaching and re-attaching
if [ "$IS_LUKS" = true ]; then
    echo "LUKS mode: skipping loop cycle and SSH injection (root is encrypted)"
    # LUKS root is inaccessible from host; SSH key was injected by
    # bootc install to-disk --root-ssh-authorized-keys
    # Skip injection/fixtures/BLS steps: jump to OVMF setup
fi

if [ "$IS_LUKS" != true ]; then
    echo "Cycling loop device to refresh partitions..."
fi
sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
LOOP_DEV=$(sudo losetup --show -f -P disk.raw)
echo "Re-attached loop device: $LOOP_DEV"

# 5. Inject SSH keys and configuration (skip for LUKS — root is encrypted)
if [ "$IS_LUKS" = true ]; then
    echo "LUKS mode: SSH key already injected by bootc install, skipping host-side injection"
else
step "=== Injecting SSH credentials to disk image ==="
# Find the root partition (not the ESP/vfat).
ROOT_PART=""
# udev / partition scanning can be asynchronous; retry with partprobe + settle
for i in $(seq 1 10); do
    sudo partprobe "$LOOP_DEV" 2>/dev/null || true
    sudo udevadm settle 2>/dev/null || true
    for p in "${LOOP_DEV}"p*; do
        local_fstype=$(sudo blkid -o value -s TYPE "$p" 2>/dev/null || true)
        # This block only runs for non-LUKS filesystems (the xfs+crypt case
        # returns early above), so match the plain root filesystem directly.
        if [ "$local_fstype" = "$FILESYSTEM" ]; then
            ROOT_PART="$p"
            break 2
        fi
    done
    sleep 1
done
if [ -z "$ROOT_PART" ]; then
    echo "ERROR: could not find root partition on $LOOP_DEV (fs=$FILESYSTEM)" >&2
    sudo losetup -d "$LOOP_DEV"; exit 1
fi
MNT_DIR="/tmp/mnt-e2e-disk"
sudo mkdir -p "$MNT_DIR"
sudo mount "$ROOT_PART" "$MNT_DIR"

# Wait a second for mount to settle
sleep 1

# Inject SSH key to root home (which is symlinked to /var/roothome on OSTree)
ROOT_SSH_DIR="$MNT_DIR/ostree/deploy/default/var/roothome/.ssh"
sudo mkdir -p "$ROOT_SSH_DIR"
sudo chmod 700 "$ROOT_SSH_DIR"
sudo cp ./test_key.pub "$ROOT_SSH_DIR/authorized_keys"
sudo chmod 600 "$ROOT_SSH_DIR/authorized_keys"
sudo chown -R 0:0 "$ROOT_SSH_DIR"

# Ensure SSH permits root login (already in derived image, but double-check)
SSHD_CONFIG_DIR="$MNT_DIR/ostree/deploy/default/var/etc/ssh"
sudo mkdir -p "$SSHD_CONFIG_DIR/sshd_config.d"
echo "PermitRootLogin yes" | sudo tee "$SSHD_CONFIG_DIR/sshd_config.d/90-e2e.conf" >/dev/null 2>&1 || true

# Add e2e-sshd.socket for TCP 22 (Bluefin's sshd only binds Unix-local + vsock).
# Written to the live /etc so it's in `cur` but NOT in the OSTree factory `old`.
# This ensures the ComposeFS 3-way merge treats it as a user-created file and
# preserves it across migration.
ETC_SYSTEMD="$MNT_DIR/ostree/deploy/default/var/etc/systemd/system"
sudo mkdir -p "$ETC_SYSTEMD/sockets.target.wants"
sudo tee "$ETC_SYSTEMD/e2e-sshd.socket" >/dev/null <<'SOCKETEOF'
[Unit]
Description=E2E SSH TCP Socket (port 22)
[Socket]
ListenStream=22
Accept=yes
[Install]
WantedBy=sockets.target
SOCKETEOF
sudo tee "$ETC_SYSTEMD/e2e-sshd@.service" >/dev/null <<'SERVICEEOF'
[Unit]
Description=E2E SSH per-connection service
[Service]
ExecStart=-/usr/sbin/sshd -i
StandardInput=socket
SERVICEEOF
sudo ln -sf ../e2e-sshd.socket \
    "$ETC_SYSTEMD/sockets.target.wants/e2e-sshd.socket"

# /var state-preservation fixtures are created live over SSH on the booted
# source system (see the ETCFIX block below), not pre-staged here: bootc/ostree
# first-boot /var setup does not preserve arbitrary pre-staged /var content.
fi

MNT_DIR="${MNT_DIR:-/tmp/mnt-e2e-disk}"

# Inject console=ttyS0 into BLS entries so the kernel logs to serial (visible
# in qemu.log). Without this, desktop-flavored images like Bluefin send kernel
# output only to the graphical console and we have zero visibility post-GRUB.

step "=== Patching BLS entries for serial console visibility ==="
sudo umount "$MNT_DIR" || true
BOOT_MNT="/tmp/mnt-e2e-boot"
sudo mkdir -p "$BOOT_MNT"
PATCHED=0
for part in "${LOOP_DEV}p1" "${LOOP_DEV}p2" "${LOOP_DEV}p3" "${LOOP_DEV}p4"; do
    [ -b "$part" ] || continue
    sudo mount "$part" "$BOOT_MNT" 2>/dev/null || continue
    # The BLS entries dir may live at the partition root (dedicated /boot
    # partition) or at /boot/loader/entries (root partition).
    for entries in "$BOOT_MNT/loader/entries" "$BOOT_MNT/boot/loader/entries"; do
        if [ -d "$entries" ]; then
            echo "Found BLS entries at $entries (on $part)"
            for conf in "$entries"/*.conf; do
                [ -f "$conf" ] || continue
                if ! grep -q 'console=ttyS0' "$conf"; then
                    sudo sed -i 's|^\(options .*\)$|\1 console=ttyS0,115200n8 console=tty0 systemd.log_level=info|' "$conf" 2>/dev/null || true
                    echo "  patched: $(basename "$conf")"
                    PATCHED=$((PATCHED + 1))
                fi
            done
        fi
    done
    sudo umount "$BOOT_MNT"
done
sudo rmdir "$BOOT_MNT"
echo "Patched $PATCHED BLS entries with serial console kernel args."

# Unmount loop device
sudo rmdir "$MNT_DIR" 2>/dev/null || true
sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
# Reset loop variable so cleanup doesn't try to double-detach
LOOP_DEV=""
echo "Disk image initialized and customized."
# Save checkpoint for faster re-runs (skip disk creation + install).
cp disk.raw "$CHECKPOINT"

# 6. Launch QEMU VM
step "=== Booting VM under QEMU ==="
KVM_FLAG=""
CPU_FLAG="-cpu max"
if [ -e /dev/kvm ]; then
    KVM_FLAG="-enable-kvm"
    # With KVM, expose host CPU features — the Rust binary is built on the
    # host and assumes x86-64-v3 (AVX2 etc.), which the default qemu64 CPU
    # does not provide and the guest aborts with "CPU ISA level is lower
    # than required".
    CPU_FLAG="-cpu host"
    echo "KVM acceleration enabled; CPU=host."
else
    echo "KVM not available. Falling back to emulation mode (TCG); CPU=max."
fi

# OVMF NVRAM persistence: without a writable VARS pflash, every QEMU boot starts
# with an empty NVRAM, OVMF re-scans the ESP, and Fedora\shim wins over our
# freshly-installed \EFI\systemd\systemd-bootx64.efi. That defeats the purpose
# of `efibootmgr --create` because the new BootOrder never survives a reboot.
#
# A zeroed file is NOT a valid VARS image — OVMF treats it as uninitialised and
# keeps NVRAM in volatile memory. We need a real template with the variable-store
# header. Locate one (Fedora ships /usr/share/OVMF/OVMF_VARS_4M.fd in containers;
# upstream brew builds don't include it) and pad it to 4 MB to match the CODE
# pflash size. Cache the prepared file under workspace/ovmf_vars.fd.
OVMF_VARS="$WORKSPACE_DIR/ovmf_vars.fd"
# Always create fresh OVMF vars for full setup runs (NVRAM fills up with
# EFI boot entries from repeated installs). On checkpoint restore, reuse.
if [ "$SKIP_SETUP" = false ] || [ ! -f "$OVMF_VARS" ]; then
    sudo cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"
    sudo chown "$(id -u):$(id -g)" "$OVMF_VARS"
    chmod u+w "$OVMF_VARS"
    # OVMF expects VARS to match the pflash region size; the template may be
    # smaller than the runtime size (e.g. 528 KB header vs 4 MB pflash). Pad.
    CODE_SIZE=$(stat -c %s "$OVMF_PATH")
    truncate -s "$CODE_SIZE" "$OVMF_VARS"
fi

# For tpm2-luks the encrypted root's unlock key is sealed to a TPM2 device, so
# the VM needs an emulated TPM (swtpm) to enroll on first boot and unlock on
# every boot. Launch one and expose it via SWTPM_QEMU_ARGS (consumed in the
# QEMU command below) and SWTPM_PID (torn down in cleanup). Without this the
# boot hangs at the LUKS prompt and the test times out.
SWTPM_QEMU_ARGS=""
if [ "$IS_LUKS" = true ]; then
    if command -v swtpm >/dev/null 2>&1; then
        SWTPM_STATE_DIR=/tmp/swtpm-tpmstate
        SWTPM_SOCK=/tmp/swtpm-sock
        rm -rf "$SWTPM_STATE_DIR"
        # A stale pidfile left by a prior run (possibly owned by another uid
        # under /tmp's sticky bit) makes swtpm fail with "Could not open
        # pidfile ... Permission denied"; clear it before (re)starting.
        rm -f /tmp/swtpm.pid /tmp/swtpm-sock
        mkdir -p "$SWTPM_STATE_DIR"
        swtpm socket --tpm2 \
            --tpmstate dir="$SWTPM_STATE_DIR" \
            --ctrl type=unixio,path="$SWTPM_SOCK" \
            --daemon --pid file=/tmp/swtpm.pid
        SWTPM_PID=$(cat /tmp/swtpm.pid 2>/dev/null || true)
        SWTPM_QEMU_ARGS="-chardev socket,id=chrtpm,path=$SWTPM_SOCK -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-crb,tpmdev=tpm0"
        echo "[luks] started emulated TPM2 (swtpm pid ${SWTPM_PID:-unknown}) for VM boot"
    else
        echo "[luks] WARNING: swtpm not found — tpm2-luks root cannot unlock; VM boot will hang" >&2
    fi
fi

# For the encrypted scenario, answer the initramfs LUKS passphrase prompt over
# the serial console. `-nographic` wires the guest serial to QEMU's stdin, so we
# feed it from a FIFO that a background watcher writes to when the prompt appears.
QEMU_STDIN=/dev/null
if [ "$IS_LUKS" = true ]; then
    PW_FIFO=/tmp/e2e-qemu-stdin
    rm -f "$PW_FIFO"
    mkfifo "$PW_FIFO"
    exec 9<>"$PW_FIFO"   # hold the FIFO open so QEMU's reader never sees EOF
    QEMU_STDIN="$PW_FIFO"
    (
        # Answer the LUKS prompt on EVERY boot — the initial boot and the
        # post-migration reboot both prompt. Poll the serial log with grep
        # instead of a line-buffered `tail | read` loop: the prompt has no
        # trailing newline, so when boot stalls on it with no further output
        # (the common case on the post-migration reboot), a read-line watcher
        # never sees it and the VM hangs until the SSH-wait times out. grep
        # counts an unterminated final line, so polling catches every prompt;
        # systemd re-prompts on a failed attempt, raising the count again.
        injected=0
        while sleep 2; do
            prompts=$(grep -aic 'enter passphrase for disk root' qemu.log 2>/dev/null) || prompts=0
            if [ "$prompts" -gt "$injected" ]; then
                printf '%s\n' "$LUKS_PASSPHRASE" >&9
                injected=$((injected + 1))
                echo "[luks] injected passphrase over serial console (prompt #$injected)"
            fi
        done
    ) &
    PW_INJECTOR_PID=$!
fi

# Run QEMU in the background
# q35 + a writable VARS pflash is required for OVMF NV-variable persistence.
# On the default `pc` machine OVMF treats variable updates as volatile and
# never writes them through to the host file — BootOrder changes from inside
# the VM then vanish on reboot. q35 also matches what modern OVMF builds
# target (the brew-packaged CODE assumes q35-class chipset).
qemu-system-x86_64 \
    -machine q35 \
    -m 4096 \
    -smp 2 \
    -nographic \
    $KVM_FLAG \
    $CPU_FLAG \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_PATH" \
    -drive if=pflash,format=raw,file="$OVMF_VARS" \
    -drive file=disk.raw,format=raw,if=virtio \
    -netdev user,id=n1,hostfwd=tcp::"$SSH_PORT"-:22 \
    -device virtio-net-pci,netdev=n1 \
    ${SWTPM_QEMU_ARGS:-} < "$QEMU_STDIN" > qemu.log 2>&1 &
QEMU_PID=$!

# 7. Wait for SSH to be available
echo "Waiting for VM to boot and SSH to become available on port $SSH_PORT..."
MAX_ATTEMPTS=60
ATTEMPT=1
SSH_OPTS="-i ./test_key -p $SSH_PORT -o ConnectTimeout=2 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
SCP_OPTS="-i ./test_key -P $SSH_PORT -o ConnectTimeout=2 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

# Stream high-signal qemu serial output to stdout (full log is on disk in qemu.log).
vm_tail vm-serial &
TAIL_PID=$!

WAIT_START=$SECONDS
while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
        step "VM accessible via SSH after $((SECONDS - WAIT_START))s."
        kill "$TAIL_PID" 2>/dev/null || true
        TAIL_PID=""
        break
    fi
    # Emit a heartbeat every 5 attempts (~15s) so CI sees forward progress.
    if [ $((ATTEMPT % 5)) -eq 0 ]; then
        step "still waiting for SSH ($((SECONDS - WAIT_START))s elapsed, attempt $ATTEMPT/$MAX_ATTEMPTS)"
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not become accessible via SSH within time limits."
    step "=== QEMU logs ==="
    cat qemu.log || true
    exit 1
fi

# 8. The migration pulls the target image directly from its upstream registry
# (RegistryEndpoint streams layers and does the ghcr.io bearer-token dance — see
# src/migration/registry.rs::probe_v2). No local registry mirror: mirroring both
# ~9 GB images cost ~35 min in CI and timed the job out before the e2e ran.
VM_TARGET_IMAGE="$TARGET_IMAGE"
step "=== Target image: ${VM_TARGET_IMAGE} (pulled directly by the migration) ==="

# Tracer mode (#63): verify bootc-rebase route resolution on a real
# OSTree-booted system, then exit before any migration machinery runs.
if [ "$E2E_MODE" = "ostree-rebase-plan" ]; then
    step "=== ostree-rebase-plan: copying bootc-rebase to VM ==="
    scp $SCP_OPTS target/debug/bootc-rebase root@localhost:/var/tmp/bootc-rebase
    step "=== ostree-rebase-plan: resolving route on the VM ==="
    PLAN_OUT=$(ssh $SSH_OPTS root@localhost \
        "/var/tmp/bootc-rebase --target-image '$VM_TARGET_IMAGE' --target-backend ostree --plan" 2>&1) || {
        echo "FAIL: bootc-rebase --plan exited nonzero"
        echo "$PLAN_OUT"
        exit 1
    }
    echo "$PLAN_OUT"
    if ! echo "$PLAN_OUT" | grep -q 'Route: ostree -> ostree via OstreeDeploy'; then
        echo "FAIL: expected 'Route: ostree -> ostree via OstreeDeploy' in plan output"
        exit 1
    fi
    step "=== ostree-rebase-plan PASSED ==="
    exit 0
fi

# Full ostree→ostree re-base mode (#63/#64): stage the target as a plain
# OSTree deployment via Strategy::OstreeDeploy, reboot into it, and assert
# the image switched while /etc edits and /var data survived (OSTree's
# native 3-way /etc merge + shared /var).
if [ "$E2E_MODE" = "ostree-rebase" ]; then
    step "=== ostree-rebase: copying bootc-rebase to VM ==="
    scp $SCP_OPTS target/debug/bootc-rebase root@localhost:/var/tmp/bootc-rebase

    step "=== ostree-rebase: injecting /etc + /var fixtures ==="
    ssh $SSH_OPTS root@localhost bash <<'REBASEFIX'
set -e
mkdir -p /etc/rebase-test
echo "etc-rebase-value" > /etc/rebase-test/marker.conf
echo "# e2e rebase marker" >> /etc/hostname
mkdir -p /var/rebase-test
echo "var-rebase-value" > /var/rebase-test/marker.txt
REBASEFIX

    # The guest cannot reach ghcr.io for the target-image SCAN (#191) — bootc
    # switch pulls fine, the scan path does not — so gate_cross_base returns
    # Unknown here. Since #191 that is a refusal rather than a silent
    # proceed, which is the whole point: a gate that cannot see must not wave
    # things through.
    #
    # Prove it fires before opting out of it. The refusal happens before any
    # staging work, so this costs seconds, and it is the only live coverage
    # that the gate actually gates — the unit tests cover the policy, not the
    # wiring.
    # The scan path (curl -> ghcr.io) has failed inside the guest while
    # `bootc switch` pulled fine moments later, which blocked both #187 and
    # #188 without ever naming a cause. Probe it here so the run reports the
    # truth rather than leaving it to be inferred from a downstream warning.
    step "=== ostree-rebase: probing the guest's registry reachability (#187) ==="
    ssh $SSH_OPTS root@localhost '
        echo "curl binary: $(command -v curl || echo MISSING)"
        if command -v curl >/dev/null; then
            code=$(curl -sS -o /dev/null -w "%{http_code}" --max-time 20 https://ghcr.io/v2/ 2>&1) \
                && echo "ghcr.io/v2/ HTTP status: $code (401 is expected: it means reachable)" \
                || echo "ghcr.io/v2/ curl FAILED: $code"
        fi
        echo "resolv.conf: $(tr "\n" " " < /etc/resolv.conf 2>/dev/null || echo MISSING)"
    ' 2>&1 | sed 's/^/[registry-probe] /' || true

    # #191: the gate must REFUSE without an explicit opt-in. It has two valid
    # ways to refuse, and which one fires depends on whether the target scan
    # reached the registry:
    #
    #   - scan succeeded, pair is cross-base -> "Cross-base re-base detected"
    #   - scan failed, status unknown        -> "Cannot determine whether ..."
    #
    # Both satisfy #191. Asserting only the second was correct while the
    # registry scan was broken, but it started failing the moment the scan
    # began working (the token-scope fix): the gate refused for the *better*
    # reason and the assertion called that a failure. What must never happen is
    # the re-base proceeding unguarded, so that is what this checks.
    step "=== ostree-rebase: asserting the cross-base gate refuses without opt-in (#191) ==="
    GATE_OUT=$(ssh $SSH_OPTS root@localhost \
        "/var/tmp/bootc-rebase --target-image '$VM_TARGET_IMAGE' --target-backend ostree" 2>&1 || true)
    if echo "$GATE_OUT" | grep -q "Cross-base re-base detected"; then
        echo "OK: target scanned cleanly and the pair IS cross-base; gate refused."
        echo "    The remap report above is #67's code executing for real."
        CROSS_BASE_EXECUTED=1
    elif echo "$GATE_OUT" | grep -q "Cannot determine whether this is a cross-base re-base"; then
        echo "OK: target could not be scanned; gate refused on unknown status (#191)."
        echo "    NOTE: is_cross_base was never evaluated, so this run proves the"
        echo "    gate's wiring but not the remap path. See the [registry-probe]"
        echo "    output above for why the scan failed."
        CROSS_BASE_EXECUTED=0
    else
        echo "FAIL: the cross-base gate did not refuse."
        echo "      Without an explicit --accept-cross-base the re-base must not"
        echo "      proceed, whether the target is known cross-base or unknown."
        echo "      Proceeding unguarded is the #191 bug."
        echo "--- bootc-rebase output ---"
        echo "$GATE_OUT" | sed 's/^/[gate] /'
        exit 1
    fi

    # These image pairs are same-lineage Fedora (bluefin:stable -> dakota /
    # aurora), and the harness knows it, so it is the operator that opts in.
    # That is what --accept-cross-base is for; it is not a way around the gate.
    REBASE_FLAGS="--accept-cross-base"

    # #188: on the cross-DE cell, also ask for desktop migration. Without
    # --de-migrate the controller reports "skipped (--de-migrate not passed)"
    # and the stash/restore code never runs, so the cross-DE cell was
    # exercising the re-base route while proving nothing about #68.
    if [ "${E2E_DE_MIGRATE:-0}" = "1" ]; then
        REBASE_FLAGS="$REBASE_FLAGS --de-migrate"
        step "=== ostree-rebase: seeding a GNOME config to stash (#188) ==="
        ssh $SSH_OPTS root@localhost '
            set -e
            u=$(awk -F: "\$3>=1000 && \$3<65534 && \$7 !~ /nologin|false/ {print \$1; exit}" /etc/passwd)
            if [ -z "$u" ]; then echo "NO_HUMAN_USER"; exit 0; fi
            h=$(getent passwd "$u" | cut -d: -f6)
            mkdir -p "$h/.config/dconf" "$h/.local/share/gnome-shell"
            echo "e2e-gnome-marker" > "$h/.config/dconf/user"
            chown -R "$u" "$h/.config" "$h/.local" 2>/dev/null || true
            echo "SEEDED user=$u home=$h"
        ' 2>&1 | sed "s/^/[de-seed] /"
    fi

    step "=== ostree-rebase: running bootc-rebase --target-backend ostree ==="
    if ! ssh $SSH_OPTS root@localhost \
        "/var/tmp/bootc-rebase --target-image '$VM_TARGET_IMAGE' --target-backend ostree $REBASE_FLAGS" \
        > /tmp/rebase-out.log 2>&1; then
        sed 's/^/[rebase] /' /tmp/rebase-out.log
        echo "FAIL: bootc-rebase exited nonzero"
        exit 1
    fi
    sed 's/^/[rebase] /' /tmp/rebase-out.log

    # The gate above already proved this pair is cross-base by scanning it, so
    # the opted-in re-base must show the remap running. This is #187's actual
    # deliverable — the cross-base code path executing under assertion — and it
    # is available on this existing cell, without a dedicated matrix entry,
    # now that the target scan works (the token-scope fix).
    if [ "${CROSS_BASE_EXECUTED:-0}" = "1" ]; then
        step "=== ostree-rebase: asserting the cross-base remap ran after opt-in (#187) ==="
        if ! grep -q "Cross-base UID/GID remap report" /tmp/rebase-out.log; then
            echo "FAIL: the gate identified this pair as cross-base, but the"
            echo "      opted-in re-base printed no remap report. The refusal"
            echo "      path ran and the apply path did not."
            exit 1
        fi
        echo "OK: cross-base remap report emitted during the opted-in re-base."
    fi

    if [ "${E2E_CROSS_BASE:-0}" = "1" ]; then
        # The whole point of this cell is that the cross-base code RAN. Without
        # an assertion the cell would pass just as happily against a
        # same-family pair, which is how the gap stayed invisible:
        # gate_cross_base returns None and prints nothing whenever it declines
        # to act, so silence is indistinguishable from success.
        #
        # Silence has FOUR causes, though, and they are not equivalent — report
        # which one actually happened rather than naming a single guess. An
        # earlier version of this check asserted "is_cross_base was false"
        # unconditionally and was wrong: the real cause was an unreachable
        # registry, which is a different (and more interesting) problem.
        step "=== ostree-rebase: asserting the cross-base path actually executed ==="
        if grep -q "Cross-base UID/GID remap report" /tmp/rebase-out.log; then
            echo "OK: cross-base detected; remap report emitted."
        elif grep -q "could not scan target image for cross-base identity" /tmp/rebase-out.log; then
            echo "FAIL: the target image could not be scanned, so cross-base"
            echo "      detection never ran. Since #191 this is not silent —"
            echo "      the gate refused and only the explicit"
            echo "      --accept-cross-base above let the re-base continue —"
            echo "      but the code under test still did not execute. This is"
            echo "      NOT a same-family result: is_cross_base was never"
            echo "      evaluated. See the registry warning above (#191)."
            exit 1
        else
            echo "FAIL: E2E_CROSS_BASE=1 and the target scanned cleanly, but no"
            echo "      remap report was printed — is_cross_base returned false,"
            echo "      so this image pair is NOT cross-base and the code under"
            echo "      test never ran. Pick a pair whose ID/ID_LIKE genuinely"
            echo "      disagree (note that CentOS declares ID_LIKE=...fedora,"
            echo "      so EL -> Fedora is same-lineage by design)."
            exit 1
        fi
    fi

    if [ "${E2E_DE_MIGRATE:-0}" = "1" ]; then
        # Same discipline as the cross-base assertion above: the DE step is
        # silent whenever it declines to act, so an unasserted cell passes
        # vacuously. Silence has FOUR distinct causes here — report which one
        # happened rather than naming a single guess.
        step "=== ostree-rebase: asserting DE stash/restore actually ran (#188) ==="
        if grep -qE "DE migration: (gnome|kde|cosmic|niri|xfce) -> " /tmp/rebase-out.log; then
            echo "OK: cross-desktop migration planned and executed."

            # The plan alone is not the deliverable — #68's unshipped half is
            # the stash itself. Assert the outgoing config actually moved.
            if ! ssh $SSH_OPTS root@localhost '
                u=$(awk -F: "\$3>=1000 && \$3<65534 && \$7 !~ /nologin|false/ {print \$1; exit}" /etc/passwd)
                h=$(getent passwd "$u" | cut -d: -f6)
                test -d "$h/.local/share/de-migrate"
            '; then
                echo "FAIL: DE migration reported a plan, but no stash directory"
                echo "      exists at ~/.local/share/de-migrate. The plan ran and"
                echo "      the move did not — that is #68's unshipped half."
                exit 1
            fi
            echo "OK: stash directory present; outgoing DE config was moved."
        elif grep -q "skipped (--de-migrate not passed)" /tmp/rebase-out.log; then
            echo "FAIL: --de-migrate was in REBASE_FLAGS but the controller still"
            echo "      reported it as not passed — the flag is not reaching the"
            echo "      controller. This is a wiring bug, not a policy decision."
            exit 1
        elif grep -q "nothing to do (this host:" /tmp/rebase-out.log; then
            echo "FAIL: the host and target resolved to the same desktop, so the"
            echo "      cross-DE code never ran. E2E_DE_MIGRATE=1 belongs only on"
            echo "      a genuinely cross-DE pair (bluefin GNOME -> aurora KDE)."
            exit 1
        elif grep -q "no human accounts to stash" /tmp/rebase-out.log; then
            echo "FAIL: no human account was found, so there was nothing to stash."
            echo "      The seeding step above should have created one; check the"
            echo "      [de-seed] output for NO_HUMAN_USER."
            exit 1
        else
            echo "FAIL: E2E_DE_MIGRATE=1 but no DE migration line appeared at all."
            echo "      The most likely cause is that detect_image_desktop could"
            echo "      not scan the target image — the same registry path that"
            echo "      blocks #187. See the [registry-probe] output above."
            exit 1
        fi
    fi

    if [ "${E2E_BOOT_ENTRIES:-0}" = "1" ]; then
        # #189: until this ran, no cell mutated NVRAM, so #31's efibootmgr
        # executor had no live coverage at all — only unit tests of the plan.
        # The apply/undo round trip is what proves the executor and its
        # snapshot restore actually work against real firmware variables.
        #
        # Scope note: #189 also names #65's live bootloader flip. That is NOT
        # covered here and cannot be — `migrate-bootloader` currently bails
        # with "not implemented yet (issue #65)", so there is no flip to test.
        step "=== boot-entries: audit is read-only and machine-readable (#31) ==="
        if ! ssh $SSH_OPTS root@localhost \
            "/var/tmp/bootc-rebase boot-entries --json" > /tmp/be-audit.json 2>/tmp/be-audit.err; then
            sed 's/^/[boot-entries] /' /tmp/be-audit.err
            echo "FAIL: boot-entries --json exited nonzero"
            exit 1
        fi
        if ! python3 -c "import json,sys; d=json.load(open('/tmp/be-audit.json')); sys.exit(0 if d else 1)"; then
            echo "FAIL: boot-entries --json did not produce a non-empty audit."
            echo "      Either NVRAM has no entries (OVMF VARS not persisted —"
            echo "      see the #189 pflash setup) or the JSON shape changed."
            sed 's/^/[audit] /' /tmp/be-audit.json | head -20
            exit 1
        fi
        echo "OK: audit returned entries as JSON."

        BEFORE=$(ssh $SSH_OPTS root@localhost "efibootmgr -v" 2>/dev/null || true)
        if [ -z "$BEFORE" ]; then
            echo "FAIL: efibootmgr returned nothing — this guest has no UEFI"
            echo "      variables, so the NVRAM path cannot be exercised."
            exit 1
        fi

        step "=== boot-entries: apply + undo round trip restores NVRAM (#31) ==="
        # --rename-branding gives the planner something to actually do on a
        # stock image (generic "Linux"/"UEFI OS" labels), so the executor is
        # exercised rather than no-oping.
        ssh $SSH_OPTS root@localhost \
            "/var/tmp/bootc-rebase boot-entries --rename-branding --apply --yes" \
            > /tmp/be-apply.log 2>&1 || true
        sed 's/^/[apply] /' /tmp/be-apply.log | tail -20

        if ! ssh $SSH_OPTS root@localhost "ls /var/lib/bootc-rebase/nvram-*.json" >/dev/null 2>&1; then
            echo "FAIL: --apply left no NVRAM snapshot behind. The snapshot is"
            echo "      the only way back from a bad write; without it --undo"
            echo "      has nothing to restore."
            exit 1
        fi
        echo "OK: pre-change NVRAM snapshot was written."

        if ! ssh $SSH_OPTS root@localhost \
            "/var/tmp/bootc-rebase boot-entries --undo --yes" > /tmp/be-undo.log 2>&1; then
            sed 's/^/[undo] /' /tmp/be-undo.log
            echo "FAIL: boot-entries --undo exited nonzero; NVRAM may be left"
            echo "      in the applied state. The snapshot is at"
            echo "      /var/lib/bootc-rebase/nvram-*.json on the guest."
            exit 1
        fi
        sed 's/^/[undo] /' /tmp/be-undo.log | tail -10

        AFTER=$(ssh $SSH_OPTS root@localhost "efibootmgr -v" 2>/dev/null || true)
        if [ "$BEFORE" != "$AFTER" ]; then
            echo "FAIL: NVRAM did not return to its pre-apply state after --undo."
            echo "--- before ---"; echo "$BEFORE"
            echo "--- after ----"; echo "$AFTER"
            exit 1
        fi
        echo "OK: NVRAM restored exactly; #31's executor and its undo path both ran live."
    fi

    step "=== ostree-rebase: verifying staged deployment ==="
    if ! ssh $SSH_OPTS root@localhost "bootc status --json" \
        | grep -q "$(echo "$VM_TARGET_IMAGE" | sed 's|.*/||')"; then
        echo "FAIL: staged deployment does not reference $VM_TARGET_IMAGE"
        exit 1
    fi

    # `bootc switch`'s native OSTree 3-way merge treats a file baked into the
    # BASE image (present in old-default, unchanged in current, absent from
    # the target's new-default) as vendor content that was dropped upstream,
    # and drops it too — exactly the semantics core::mergetc mirrors for the
    # composefs path (see deploy.rs::ensure_e2e_ssh_socket, which recreates
    # this same file for that path). e2e-sshd.socket baked into the base
    # image is invisible to the target's own defaults, so it does NOT
    # survive the switch if injected pre-switch (confirmed: it disappears
    # from the staged deploy's /etc post-merge). Inject it post-merge,
    # directly into the *staged* deployment's /etc, mirroring
    # ensure_e2e_ssh_socket's timing rather than the merge's.
    step "=== ostree-rebase: re-injecting e2e-sshd into the staged deploy ==="
    ssh $SSH_OPTS root@localhost bash <<'POSTMERGEFIX'
set -e
# Exactly two deployments exist at this point (booted + staged); the
# booted one is marked with a leading '*' in `ostree admin status`, so the
# other line is unambiguously the staged deployment.
DEPLOY_LINE=$(ostree admin status | grep -v '^\*' | grep -v '^ *$' | head -1)
STATEROOT=$(echo "$DEPLOY_LINE" | awk '{print $1}')
CKSUM_SERIAL=$(echo "$DEPLOY_LINE" | awk '{print $2}')
DEPLOY_ETC="/ostree/deploy/${STATEROOT}/deploy/${CKSUM_SERIAL}/etc"
[ -d "$DEPLOY_ETC" ] || { echo "FAIL: staged deployment etc dir not found ($DEPLOY_ETC)"; exit 1; }

mkdir -p "$DEPLOY_ETC/systemd/system/sockets.target.wants"
printf '%s\n' \
    '[Unit]' \
    'Description=E2E SSH TCP Socket (port 22)' \
    '[Socket]' \
    'ListenStream=22' \
    'Accept=yes' \
    '[Install]' \
    'WantedBy=sockets.target' \
    > "$DEPLOY_ETC/systemd/system/e2e-sshd.socket"
printf '%s\n' \
    '[Unit]' \
    'Description=E2E SSH per-connection service' \
    '[Service]' \
    'ExecStart=-/usr/sbin/sshd -i' \
    'StandardInput=socket' \
    > "$DEPLOY_ETC/systemd/system/e2e-sshd@.service"
ln -sf ../e2e-sshd.socket "$DEPLOY_ETC/systemd/system/sockets.target.wants/e2e-sshd.socket"

# Belt and suspenders, mirroring deploy.rs::ensure_e2e_ssh_socket's own
# defensive removal: having both sshd.service (sshd -D) and e2e-sshd.socket
# bound to port 22 kills the daemon with 255/EXCEPTION.
rm -f "$DEPLOY_ETC/systemd/system/multi-user.target.wants/sshd.service"
POSTMERGEFIX

    step "=== ostree-rebase: rebooting into the new deployment ==="
    ssh $SSH_OPTS root@localhost "reboot" || true
    sleep 5
    ATTEMPT=1
    WAIT_START=$SECONDS
    while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
        if ssh $SSH_OPTS root@localhost true 2>&1; then
            step "VM accessible via SSH after re-base reboot ($((SECONDS - WAIT_START))s)."
            break
        fi
        if [ $((ATTEMPT % 5)) -eq 0 ]; then
            step "still waiting for post-rebase SSH ($((SECONDS - WAIT_START))s elapsed, attempt $ATTEMPT/$MAX_ATTEMPTS)"
        fi
        sleep 3
        ATTEMPT=$((ATTEMPT + 1))
    done
    if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
        echo "ERROR: VM did not boot back after the ostree re-base."
        grep -E '\[FAILED\]|DEPEND\]' qemu.log | tail -40 || true
        exit 1
    fi

    step "=== ostree-rebase: post-reboot assertions ==="
    # Deliberately unquoted heredoc: $VM_TARGET_IMAGE must expand client-side
    # (it's a local variable, not present in the SSH'd shell); \$-escaped
    # tokens below expand server-side instead.
    # shellcheck disable=SC2087
    ssh $SSH_OPTS root@localhost bash <<REBASECHECK
set -e
booted=\$(bootc status --json | python3 -c "import json,sys; print(json.load(sys.stdin)['status']['booted']['image']['image']['image'])")
case "\$booted" in
  *$(echo "$VM_TARGET_IMAGE" | sed 's|.*/||')*) echo "OK: booted image is \$booted" ;;
  *) echo "FAIL: booted image is \$booted, expected $VM_TARGET_IMAGE"; exit 1 ;;
esac
grep -q "etc-rebase-value" /etc/rebase-test/marker.conf || { echo "FAIL: /etc marker lost"; exit 1; }
grep -q "e2e rebase marker" /etc/hostname || { echo "FAIL: /etc/hostname edit lost"; exit 1; }
grep -q "var-rebase-value" /var/rebase-test/marker.txt || { echo "FAIL: /var marker lost"; exit 1; }
# Previous deployment must remain as rollback.
rollback=\$(bootc status --json | python3 -c "import json,sys; print(bool(json.load(sys.stdin)['status'].get('rollback')))")
[ "\$rollback" = "True" ] || { echo "FAIL: no rollback deployment after re-base"; exit 1; }
echo "OK: /etc + /var preserved, rollback deployment present"

# #80: bootc switch's native ostree 3-way /etc merge has no identity-DB-aware
# special-casing the way mergetc.rs does for the composefs conversion path
# (union-merge by key so e.g. a target-only 'messagebus' system account
# survives even when old==cur locally). A divergent system-user set between
# source and target images (different DE spins, different dbus stack) could
# in principle drop an account the merge doesn't know is load-bearing, which
# manifests as dbus/logind failing to start post-reboot. Assert the bus
# actually works rather than just checking the deployment staged/booted.
if systemctl is-active --quiet dbus.service || systemctl is-active --quiet dbus-broker.service; then
    echo "OK: system dbus is active."
else
    echo "FAIL: neither dbus.service nor dbus-broker.service is active post-rebase (#80)"; exit 1
fi
busctl list --system >/dev/null 2>&1 || { echo "FAIL: system bus is not queryable post-rebase (#80)"; exit 1; }
loginctl list-sessions >/dev/null 2>&1 || { echo "FAIL: systemd-logind (via dbus) is not responding post-rebase (#80)"; exit 1; }
# (sshd itself is proven by the fact this script is SSH'd in to run these
# checks at all — no separate assertion needed.)
echo "OK: dbus/logind healthy post-rebase (#80 identity-DB regression check)"
REBASECHECK

    step "=== ostree-rebase PASSED ==="
    exit 0
fi

step "=== Copying migration utility to VM ==="
scp $SCP_OPTS target/debug/bootc-migrate root@localhost:/var/tmp/bootc-migrate

step "=== Injecting /etc fixtures (live, copied by migration) ==="
ssh $SSH_OPTS root@localhost bash <<'ETCFIX'
set -e
# Custom config file in /etc to verify /etc state is preserved through migration
mkdir -p /etc/migration-test
echo "etc-state-value" > /etc/migration-test/marker.conf
echo "nested-etc-value" > /etc/migration-test/nested.conf
# Modify an existing /etc file to verify in-place edits are preserved
echo "# e2e migration marker" >> /etc/hostname
# A symlink in /etc to verify symlink handling
ln -sf marker.conf /etc/migration-test/marker.link

# Real user account so /home/<user> -> /var/home/<user> path resolution is tested
useradd -m -U realuser 2>/dev/null || true
mkdir -p /var/home/realuser
echo "real-home-data" > /var/home/realuser/home-marker.txt
chown -R realuser:realuser /var/home/realuser

# Persist absolute legacy paths like real OSTree user state does. A native
# /home target must keep these working without rewriting the user's files.
mkdir -p /var/home/realuser/.config/legacy-home /var/home/realuser/.local/bin
ln -s /var/home/realuser/home-marker.txt /var/home/realuser/.config/legacy-home/marker.link
ln -s /bin/sh /var/home/realuser/.local/bin/legacy-home-shell
printf '#!/var/home/realuser/.local/bin/legacy-home-shell\necho legacy-home-exec\n' \
    > /var/home/realuser/.local/bin/legacy-home-script
chmod 755 /var/home/realuser/.local/bin/legacy-home-script

# Full-fat /etc config drift (#23): realistic per-machine edits a desktop
# user makes which the migration MUST preserve.
mkdir -p /etc/sudoers.d
echo "realuser ALL=(ALL) NOPASSWD: /usr/bin/dnf" > /etc/sudoers.d/90-realuser
chmod 440 /etc/sudoers.d/90-realuser
echo "10.0.0.42  e2e-migration-test.local" >> /etc/hosts
mkdir -p /etc/ssh/sshd_config.d
printf 'X11Forwarding yes\nClientAliveInterval 60\n' > /etc/ssh/sshd_config.d/99-local.conf

# Full-fat user state (#23): drop the kind of files a real Bluefin
# desktop accumulates — wallpaper, GNOME extension, dconf db / gsettings
# keyfile (accent color, dark mode, wallpaper URI, custom keybinding),
# homebrew prefix, flatpak user + system installs.
mkdir -p /var/home/realuser/Pictures
printf 'PNGFAKE\xfe\x00wallpaper-bytes\n' > /var/home/realuser/Pictures/migration-wallpaper.png
mkdir -p /var/home/realuser/.local/share/gnome-shell/extensions/migration-test@e2e
cat > /var/home/realuser/.local/share/gnome-shell/extensions/migration-test@e2e/metadata.json <<'EXTMETA'
{
  "name": "Migration Test Extension",
  "uuid": "migration-test@e2e",
  "version": 1,
  "shell-version": ["45", "46"]
}
EXTMETA
echo "// migration-test extension stub" > /var/home/realuser/.local/share/gnome-shell/extensions/migration-test@e2e/extension.js
mkdir -p /var/home/realuser/.config/dconf
echo "DCONF-USER-DB-SENTINEL" > /var/home/realuser/.config/dconf/user
mkdir -p /var/home/realuser/.config/glib-2.0/settings
cat > /var/home/realuser/.config/glib-2.0/settings/keyfile <<'GSETTINGS'
[org/gnome/desktop/interface]
accent-color='blue'
color-scheme='prefer-dark'

[org/gnome/desktop/background]
picture-uri='file:///var/home/realuser/Pictures/migration-wallpaper.png'

[org/gnome/desktop/wm/keybindings]
switch-windows=['<Alt>Tab']
GSETTINGS
mkdir -p /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/bin
printf '#!/bin/sh\necho jq stub\n' > /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/bin/jq
chmod 755 /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/bin/jq
echo '{"name":"jq","version":"1.7.1"}' > /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/INSTALL_RECEIPT.json
mkdir -p /var/home/realuser/.local/share/flatpak/app/org.gnome.Calculator/current/active
echo "flatpak-user-stub-org.gnome.Calculator" > /var/home/realuser/.local/share/flatpak/app/org.gnome.Calculator/current/active/metadata
mkdir -p /var/lib/flatpak/app/com.example.SystemApp/current/active
echo "flatpak-system-stub-com.example.SystemApp" > /var/lib/flatpak/app/com.example.SystemApp/current/active/metadata
chown -R realuser:realuser /var/home/realuser 2>/dev/null || true

# Basic /var state-preservation fixtures. These MUST be created live on the
# running system (not pre-staged to the disk before first boot): bootc/ostree
# first-boot /var setup does not preserve arbitrary pre-staged /var content, so
# pre-staging gives a false negative. The migration captures live /var data.
mkdir -p /var/lib/migration-test
echo "persistent-test-data-value" > /var/lib/migration-test/data
echo "timestamp-$(date +%s)" > /var/lib/migration-test/created-at

mkdir -p /var/home/testuser/.config
echo "hello-user-data-value" > /var/home/testuser/user-data.txt
echo "dotfile-content" > /var/home/testuser/.config/settings.conf
chmod -R 755 /var/home/testuser

mkdir -p /var/home/devuser/projects/myapp/src /var/home/devuser/.ssh
echo "package main" > /var/home/devuser/projects/myapp/src/main.go
echo "README for myapp" > /var/home/devuser/projects/myapp/README.md
echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI..." > /var/home/devuser/.ssh/id_ed25519.pub
chmod -R 755 /var/home/devuser
chmod 700 /var/home/devuser/.ssh

mkdir -p /var/lib/systemd/timers
echo "stamp" > /var/lib/systemd/timers/test-timer

mkdir -p /var/lib/alternatives
echo "selected-option" > /var/lib/alternatives/current
ln -sf current /var/lib/alternatives/default 2>/dev/null || true

mkdir -p /var/cache/.hidden-dir
echo "hidden-file-content" > /var/cache/.hidden-dir/secret
ETCFIX

step "=== Running migration inside VM ==="
# Clean composefs state from previous runs so free-space check passes.
ssh $SSH_OPTS root@localhost "rm -rf /sysroot/composefs /sysroot/state && mkdir -p /sysroot/composefs" 2>/dev/null || true

# TUI mode: same migration, but a simulated human drives it. Upload the
# pty driver, prove the interactive Config Drift Review checklist first
# (it's read-only and, like a real "Phase 0.5", runs before migration),
# then let the wizard-driven run below replace the plain CLI invocation.
if [ "$E2E_MODE" = "tui-migrate" ]; then
    step "=== tui-migrate: uploading TUI driver ==="
    scp $SCP_OPTS "$SCRIPT_DIR/tui-e2e-driver.py" root@localhost:/var/tmp/tui-e2e-driver.py

    step "=== tui-migrate: driving etc-drift --interactive (#15 checklist) ==="
    if ! ssh $SSH_OPTS root@localhost \
        "python3 /var/tmp/tui-e2e-driver.py --mode drift --binary /var/tmp/bootc-migrate --output /var/tmp/etc-drift-manifest.json --transcript /var/tmp/tui-drift-transcript.txt --record /var/tmp/tui-drift.cast --snapshot-dir /var/tmp/tui-snapshots" \
        2>&1 | sed 's/^/[tui-drift] /'; then
        echo "FAIL: TUI drift review driver failed; final screen follows"
        ssh $SSH_OPTS root@localhost "cat /var/tmp/tui-drift-transcript.txt" 2>/dev/null || true
        exit 1
    fi
    # The /etc fixtures injected above guarantee real drift (hostname,
    # hosts, sshd_config.d, sudoers.d), and the driver unchecked exactly
    # one entry — assert the manifest recorded both kinds of decision.
    # (The manifest is deliberately NOT passed to the migration: this
    # cell must keep the default merge behavior the downstream fixture
    # assertions were written for.)
    ssh $SSH_OPTS root@localhost python3 <<'DRIFTCHECK'
import json
with open("/var/tmp/etc-drift-manifest.json") as f:
    decisions = json.load(f)["decisions"]
assert decisions, "FAIL: drift manifest has no decisions"
taken = sum(1 for keep in decisions.values() if not keep)
assert taken == 1, f"FAIL: expected exactly 1 take-target decision, got {taken}"
print(f"OK: drift manifest has {len(decisions)} decision(s), "
      f"{len(decisions) - taken} keep-current, {taken} take-target")
DRIFTCHECK
fi

# Run the migration in the background so we can interleave a heartbeat. Pipe the
# binary's output through a prefixer so its `=== Phase N ===` lines show up as
# `[migrate]` in the CI log, distinct from script-level `[e2e …]` markers.
# In tui-migrate mode the same run is driven through the TUI wizard on a
# pty instead (custom image typed in, dry-run off, skip-import + force
# toggled on — the exact configuration of the CLI invocation), so the
# `[migrate]` stream carries the driver's screen-by-screen progress.
if [ "$E2E_MODE" = "tui-migrate" ]; then
    MIGRATE_CMD="python3 /var/tmp/tui-e2e-driver.py --mode wizard --binary /var/tmp/bootc-migrate --target-image $VM_TARGET_IMAGE --transcript /var/tmp/tui-wizard-transcript.txt --record /var/tmp/tui-migrate.cast --snapshot-dir /var/tmp/tui-snapshots"
else
    MIGRATE_CMD="/var/tmp/bootc-migrate --target-image $VM_TARGET_IMAGE --force --skip-import"
fi
MIGRATE_START=$SECONDS
{
    ssh $SSH_OPTS root@localhost "$MIGRATE_CMD" 2>&1 \
      | awk '{ print "[migrate] " $0; fflush() }'
    echo "MIGRATE_RC=${PIPESTATUS[0]}" > /tmp/e2e-migrate.rc
} &
MIGRATE_BG=$!
heartbeat "$MIGRATE_BG" 20 "migration in progress" &
HB_PID=$!
wait "$MIGRATE_BG"
kill "$HB_PID" 2>/dev/null || true
HB_PID=""
MIGRATE_RC=$(cat /tmp/e2e-migrate.rc 2>/dev/null | cut -d= -f2)
rm -f /tmp/e2e-migrate.rc
step "Migration completed in $((SECONDS - MIGRATE_START))s (rc=${MIGRATE_RC:-?})"
# Pull the TUI walkthrough artifacts out of the guest: the asciicast
# recordings of both interactive flows (render a timelapse with
# `asciinema play` or `agg … .gif` — CI does the latter) and the
# per-screen text screenshots. Runs on failure too — a failed run's
# recording is exactly the debugging evidence wanted — and before the
# reboot on success. Best-effort: a failed copy must not fail an
# otherwise green migration.
fetch_tui_artifacts() {
    step "=== tui-migrate: fetching recordings + screenshots from VM ==="
    scp $SCP_OPTS root@localhost:/var/tmp/tui-migrate.cast \
        root@localhost:/var/tmp/tui-drift.cast "$WORKSPACE_DIR"/ 2>/dev/null || true
    scp $SCP_OPTS -r root@localhost:/var/tmp/tui-snapshots "$WORKSPACE_DIR"/ 2>/dev/null || true
    ls -la "$WORKSPACE_DIR"/tui-*.cast "$WORKSPACE_DIR"/tui-snapshots 2>/dev/null || true
}

if [ "${MIGRATE_RC:-1}" != "0" ]; then
    echo "ERROR: migration binary exited with rc=${MIGRATE_RC:-?}" >&2
    if [ "$E2E_MODE" = "tui-migrate" ]; then
        echo "--- TUI wizard final screen ---" >&2
        ssh $SSH_OPTS root@localhost "cat /var/tmp/tui-wizard-transcript.txt" 2>/dev/null >&2 || true
        fetch_tui_artifacts
    fi
    exit "${MIGRATE_RC:-1}"
fi

if [ "$E2E_MODE" = "tui-migrate" ]; then
    fetch_tui_artifacts
fi

# e2e-sshd.socket baked into the base image (see MODIFIED_IMAGE above) is
# vendor content absent from the target's own defaults, so core::mergetc's
# 3-way merge drops it (old==cur, new absent) — same semantics as the
# ostree-rebase path's native OSTree merge (see the "re-injecting e2e-sshd"
# block above). Recreate it directly in the staged deployment's /etc,
# post-merge, pre-reboot, rather than baking it into the target image itself
# (#18 — that would require the migration to pull a locally-built image it
# can't reach; see the TARGET_IMAGE note above).
step "=== Re-injecting e2e-sshd into the staged deployment ==="
ssh $SSH_OPTS root@localhost bash <<'CFS_POSTMERGEFIX'
set -e
DEPLOY_ETC=$(echo /sysroot/state/deploy/*/etc)
[ -d "$DEPLOY_ETC" ] || { echo "FAIL: staged deployment etc dir not found (/sysroot/state/deploy/*/etc)"; exit 1; }

mkdir -p "$DEPLOY_ETC/systemd/system/sockets.target.wants"
printf '%s\n' \
    '[Unit]' \
    'Description=E2E SSH TCP Socket (port 22)' \
    '[Socket]' \
    'ListenStream=22' \
    'Accept=yes' \
    '[Install]' \
    'WantedBy=sockets.target' \
    > "$DEPLOY_ETC/systemd/system/e2e-sshd.socket"
printf '%s\n' \
    '[Unit]' \
    'Description=E2E SSH per-connection service' \
    '[Service]' \
    'ExecStart=-/usr/sbin/sshd -i' \
    'StandardInput=socket' \
    > "$DEPLOY_ETC/systemd/system/e2e-sshd@.service"
ln -sf ../e2e-sshd.socket "$DEPLOY_ETC/systemd/system/sockets.target.wants/e2e-sshd.socket"

# Belt and suspenders: having both sshd.service (sshd -D) and e2e-sshd.socket
# bound to port 22 kills the daemon with 255/EXCEPTION.
rm -f "$DEPLOY_ETC/systemd/system/multi-user.target.wants/sshd.service"
CFS_POSTMERGEFIX

step "=== Verifying migration artifacts before reboot ==="
ssh $SSH_OPTS root@localhost bash <<'DIAG'
set +e
echo '--- Deployments ---'
ls -la /sysroot/state/deploy/
echo '--- BLS entries (list) ---'
ls -la /boot/loader/entries/
echo '--- BLS entry contents ---'
for f in /boot/loader/entries/*.conf; do
    echo ">>> $f"
    cat "$f"
    echo
done
echo '--- Boot dirs (/boot, GRUB2 path) ---'
ls -la /boot/bootc_composefs-*/ 2>/dev/null || echo 'No bootc_composefs dirs in /boot'
echo '--- ESP loader entries (systemd-boot path) ---'
for esp in /boot/efi /efi; do
    if [ -d "$esp/loader/entries" ]; then
        echo ">>> $esp/loader/entries/"
        ls -la "$esp/loader/entries/" 2>/dev/null
        for f in "$esp/loader/entries"/*.conf; do
            [ -f "$f" ] || continue
            echo ">>> $f"; cat "$f"; echo
        done
        echo ">>> $esp/loader/loader.conf"
        cat "$esp/loader/loader.conf" 2>/dev/null || echo 'missing'
        echo ">>> $esp/EFI/systemd/"
        ls -la "$esp/EFI/systemd/" 2>/dev/null || echo 'missing'
        echo ">>> $esp/EFI/BOOT/"
        ls -la "$esp/EFI/BOOT/" 2>/dev/null || echo 'missing'
    fi
done
# ESP may only mount transiently — try to mount partition labelled EFI-SYSTEM read-only for inspection.
ESP_DEV=$(lsblk -ndo NAME,PARTLABEL 2>/dev/null | awk '$2=="EFI-SYSTEM" {print "/dev/"$1}' | head -1)
if [ -n "$ESP_DEV" ] && ! mount | grep -q "$ESP_DEV"; then
    mkdir -p /tmp/esp-inspect
    if mount -o ro "$ESP_DEV" /tmp/esp-inspect 2>/dev/null; then
        echo "--- ESP inspected at $ESP_DEV ---"
        ls -la /tmp/esp-inspect/loader/entries/ 2>/dev/null
        ls -la /tmp/esp-inspect/EFI/systemd/ 2>/dev/null
        ls -la /tmp/esp-inspect/EFI/BOOT/ 2>/dev/null
        cat /tmp/esp-inspect/loader/loader.conf 2>/dev/null
        umount /tmp/esp-inspect 2>/dev/null
    fi
fi
echo '--- ComposeFS images ---'
ls -la /sysroot/composefs/images/ 2>/dev/null || echo 'No composefs images'
echo '--- /etc/default/grub ---'
cat /etc/default/grub 2>/dev/null || echo 'missing'
echo '--- grubenv (grub2-editenv list) ---'
grub2-editenv /boot/grub2/grubenv list 2>/dev/null || echo 'grub2-editenv failed'
echo '--- grub.cfg blscfg references ---'
grep -nE 'blscfg|blsdir|saved_entry|default=' /boot/grub2/grub.cfg 2>/dev/null | head -40 || echo 'no grub.cfg'
echo '--- grub.cfg head ---'
head -60 /boot/grub2/grub.cfg 2>/dev/null || echo 'no grub.cfg'
echo '--- efibootmgr ---'
efibootmgr -v 2>/dev/null || echo 'no efibootmgr'
echo '--- (source, pre-reboot) /var device + seeded data ---'
findmnt /var || echo '(no /var mount)'
ls -la /var/lib/migration-test 2>&1 || echo '(no /var/lib/migration-test on source!)'
echo '--- (source, pre-reboot) composefs deployment fstab ---'
cat /sysroot/state/deploy/*/etc/fstab 2>/dev/null | grep -E '/var|^UUID' || echo '(no composefs deployment fstab /var entry)'
DIAG

step "=== Rebooting VM ==="
ssh $SSH_OPTS root@localhost "reboot" || true

# Wait for VM to shutdown
sleep 5

# 9. Wait for VM to boot back
step "Waiting for VM to boot back after migration..."
vm_tail vm-post &
TAIL_PID=$!
ATTEMPT=1
WAIT_START=$SECONDS
while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>&1; then
        step "VM accessible via SSH after reboot ($((SECONDS - WAIT_START))s)."
        kill "$TAIL_PID" 2>/dev/null || true
        TAIL_PID=""
        break
    fi
    if [ $((ATTEMPT % 5)) -eq 0 ]; then
        step "still waiting for post-reboot SSH ($((SECONDS - WAIT_START))s elapsed, attempt $ATTEMPT/$MAX_ATTEMPTS)"
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not boot back after migration."
    step "=== Post-reboot failure diagnostics ==="
    echo "--- All FAILED/DEPEND lines ---"
    grep -E '\[FAILED\]|DEPEND\]' qemu.log | tail -80 || true
    echo ""
    echo "--- dbus-related lines ---"
    grep -iE 'dbus|messagebus|polkit|logind|machine.id' qemu.log | tail -40 || true
    echo ""
    echo "--- mount/overlay lines ---"
    grep -iE 'mount|overlay|composefs|erofs|subvol|fstab' qemu.log | tail -30 || true
    echo ""
    echo "--- Last 150 lines of full QEMU log ---"
    tail -150 qemu.log
    exit 1
fi

# 10. Run post-migration validation checks
step "=== Running post-migration validation checks ==="
ssh $SSH_OPTS root@localhost "bootc status"

# Check booted backend is composefs
BOOTED_BACKEND=$(ssh $SSH_OPTS root@localhost "bootc status --json" | jq -r '.status.booted.composefs')
if [ "$BOOTED_BACKEND" = "null" ]; then
    echo "WARNING: System booted back to OSTree instead of ComposeFS."
    echo "Checking that composefs deployment artifacts exist..."
    ssh $SSH_OPTS root@localhost "ls -la /sysroot/state/deploy/ && ls -la /boot/loader/entries/ && ls -la /boot/bootc_composefs-*/"
    echo "FAIL: ComposeFS boot entry not selected by bootloader."
    exit 1
fi
echo "OK: Booted backend is ComposeFS."

# For the dedicated-/var-LV scenario, dump exactly where /var landed before the
# persistence assertions run, so a failure pinpoints empty-LV vs shadowed-bind.
if [ "$IS_LVM" = true ]; then
    step "=== /var diagnostics (LVM scenario) ==="
    ssh $SSH_OPTS root@localhost bash <<'VARDIAG' || true
echo "--- findmnt /var ---"; findmnt /var || echo "(no /var mount)"
echo "--- mounts touching /var ---"; mount | grep -E ' /var( |/)' || true
echo "--- lsblk ---"; lsblk -o NAME,SIZE,TYPE,MOUNTPOINTS,UUID 2>/dev/null
echo "--- lvs ---"; lvs 2>/dev/null || echo "(lvs unavailable)"
echo "--- /etc/fstab ---"; cat /etc/fstab
echo "--- ls /var/lib/migration-test ---"; ls -la /var/lib/migration-test 2>&1 || true
echo "--- ls /var (top) ---"; ls -la /var | head -20
echo "--- ostree stateroot var ---"; ls -la /sysroot/state/os/default/var 2>/dev/null | head
echo "--- var LV content directly (mount it aside) ---"
vardev=$(blkid -L "" 2>/dev/null; lvs --noheadings -o lv_dm_path 2>/dev/null | grep -i var | tr -d ' ')
echo "var LV dm path: ${vardev:-unknown}"
if [ -n "$vardev" ]; then
    mkdir -p /tmp/varlv && mount -o ro "$vardev" /tmp/varlv 2>/dev/null \
        && { echo "var LV contents:"; ls -la /tmp/varlv | head; ls -la /tmp/varlv/lib/migration-test 2>&1 || true; umount /tmp/varlv; } \
        || echo "(could not mount var LV aside — likely already mounted at /var)"
fi
VARDIAG
fi

# Basic persistence
TEST_DATA_VAL=$(ssh $SSH_OPTS root@localhost "cat /var/lib/migration-test/data")
if [ "$TEST_DATA_VAL" != "persistent-test-data-value" ]; then
    echo "FAIL: Persistent /var data was not preserved! (Found: $TEST_DATA_VAL)"
    exit 1
fi
echo "OK: Persistent /var data preserved."

# User home files
TEST_USER_DATA=$(ssh $SSH_OPTS root@localhost "cat /var/home/testuser/user-data.txt")
if [ "$TEST_USER_DATA" != "hello-user-data-value" ]; then
    echo "FAIL: User home directory data was not preserved! (Found: $TEST_USER_DATA)"
    exit 1
fi
echo "OK: User home directory data preserved."

# Hidden dotfile
DOTFILE=$(ssh $SSH_OPTS root@localhost "cat /var/home/testuser/.config/settings.conf")
if [ "$DOTFILE" != "dotfile-content" ]; then
    echo "FAIL: Dotfile in .config was not preserved! (Found: $DOTFILE)"
    exit 1
fi
echo "OK: Dotfiles preserved."

# Second user with nested structure
DEV_README=$(ssh $SSH_OPTS root@localhost "cat /var/home/devuser/projects/myapp/README.md")
if [ "$DEV_README" != "README for myapp" ]; then
    echo "FAIL: Nested user project data was not preserved! (Found: $DEV_README)"
    exit 1
fi
echo "OK: Multi-user nested directory structure preserved."

# SSH keys
SSH_KEY=$(ssh $SSH_OPTS root@localhost "cat /var/home/devuser/.ssh/id_ed25519.pub")
if [ -z "$SSH_KEY" ]; then
    echo "FAIL: SSH keys were not preserved!"
    exit 1
fi
echo "OK: SSH key files preserved."

# System state
TIMER=$(ssh $SSH_OPTS root@localhost "cat /var/lib/systemd/timers/test-timer")
if [ "$TIMER" != "stamp" ]; then
    echo "FAIL: Systemd timer state was not preserved! (Found: $TIMER)"
    exit 1
fi
echo "OK: System state files preserved."

# Symlinks
SYMLINK_TARGET=$(ssh $SSH_OPTS root@localhost "readlink /var/lib/alternatives/default")
if [ "$SYMLINK_TARGET" != "current" ]; then
    echo "FAIL: Symlink was not preserved! (Found: $SYMLINK_TARGET)"
    exit 1
fi
LINKED_DATA=$(ssh $SSH_OPTS root@localhost "cat /var/lib/alternatives/default")
if [ "$LINKED_DATA" != "selected-option" ]; then
    echo "FAIL: Symlink target content was not preserved!"
    exit 1
fi
echo "OK: Symlinks preserved and functional."

# Hidden directory
HIDDEN=$(ssh $SSH_OPTS root@localhost "cat /var/cache/.hidden-dir/secret")
if [ "$HIDDEN" != "hidden-file-content" ]; then
    echo "FAIL: Hidden directory data was not preserved! (Found: $HIDDEN)"
    exit 1
fi
echo "OK: Hidden directory data preserved."

# /etc state preserved through migration
ETC_MARKER=$(ssh $SSH_OPTS root@localhost "cat /etc/migration-test/marker.conf 2>/dev/null || echo MISSING")
if [ "$ETC_MARKER" != "etc-state-value" ]; then
    echo "FAIL: /etc custom config was not preserved! (Found: $ETC_MARKER)"
    exit 1
fi
echo "OK: /etc custom config preserved."

ETC_NESTED=$(ssh $SSH_OPTS root@localhost "cat /etc/migration-test/nested.conf 2>/dev/null || echo MISSING")
if [ "$ETC_NESTED" != "nested-etc-value" ]; then
    echo "FAIL: Nested /etc file was not preserved! (Found: $ETC_NESTED)"
    exit 1
fi
echo "OK: Nested /etc files preserved."

# In-place edit of pre-existing /etc file
HOSTNAME_TAIL=$(ssh $SSH_OPTS root@localhost "tail -n1 /etc/hostname")
if [ "$HOSTNAME_TAIL" != "# e2e migration marker" ]; then
    echo "FAIL: Edit to existing /etc file was not preserved! (Tail: $HOSTNAME_TAIL)"
    exit 1
fi
echo "OK: In-place /etc edits preserved."

# Symlink within /etc
ETC_LINK=$(ssh $SSH_OPTS root@localhost "readlink /etc/migration-test/marker.link")
if [ "$ETC_LINK" != "marker.conf" ]; then
    echo "FAIL: /etc symlink was not preserved! (Found: $ETC_LINK)"
    exit 1
fi
echo "OK: /etc symlinks preserved."

# /home resolution, including persisted absolute /var/home paths when the
# target uses a native /home directory.
HOME_DATA=$(ssh $SSH_OPTS root@localhost "cat /home/realuser/home-marker.txt 2>/dev/null || echo MISSING")
if [ "$HOME_DATA" != "real-home-data" ]; then
    echo "FAIL: /home/<user> data was not accessible after migration! (Found: $HOME_DATA)"
    exit 1
fi
LEGACY_HOME_LINK=$(ssh $SSH_OPTS root@localhost "cat /home/realuser/.config/legacy-home/marker.link 2>/dev/null || echo MISSING")
if [ "$LEGACY_HOME_LINK" != "real-home-data" ]; then
    echo "FAIL: absolute /var/home symlink did not resolve through /home! (Found: $LEGACY_HOME_LINK)"
    exit 1
fi
LEGACY_HOME_EXEC=$(ssh $SSH_OPTS root@localhost "/home/realuser/.local/bin/legacy-home-script 2>/dev/null || echo MISSING")
if [ "$LEGACY_HOME_EXEC" != "legacy-home-exec" ]; then
    echo "FAIL: absolute /var/home shebang did not execute through /home! (Found: $LEGACY_HOME_EXEC)"
    exit 1
fi
echo "OK: /home and legacy /var/home paths resolve to the preserved user data."

# Real user account still exists
REALUSER_ENT=$(ssh $SSH_OPTS root@localhost "getent passwd realuser || echo MISSING")
if [ "$REALUSER_ENT" = "MISSING" ]; then
    echo "FAIL: realuser account missing from passwd (added via /etc/passwd edit pre-migration)"
    exit 1
fi
echo "OK: User account from /etc/passwd preserved: $REALUSER_ENT"

# --- Full-fat user state assertions (#23) ---
step "=== Running full-fat user state validation ==="

# /etc/sudoers.d/<name> — per-machine privilege drift
SUDOERS_LINE=$(ssh $SSH_OPTS root@localhost "cat /etc/sudoers.d/90-realuser 2>/dev/null")
if [ "$SUDOERS_LINE" != "realuser ALL=(ALL) NOPASSWD: /usr/bin/dnf" ]; then
    echo "FAIL: /etc/sudoers.d/90-realuser not preserved (got: $SUDOERS_LINE)"; exit 1
fi
SUDOERS_MODE=$(ssh $SSH_OPTS root@localhost "stat -c '%a' /etc/sudoers.d/90-realuser")
if [ "$SUDOERS_MODE" != "440" ]; then
    echo "FAIL: /etc/sudoers.d/90-realuser mode changed (expected 440, got $SUDOERS_MODE)"; exit 1
fi
echo "OK: /etc/sudoers.d entry preserved with 440 mode."

# /etc/hosts append
HOSTS_GREP=$(ssh $SSH_OPTS root@localhost "grep -c 'e2e-migration-test.local' /etc/hosts || true")
if [ "$HOSTS_GREP" -lt 1 ]; then
    echo "FAIL: /etc/hosts custom entry was not preserved"; exit 1
fi
echo "OK: /etc/hosts append preserved."

# /etc/ssh/sshd_config.d/99-local.conf
SSHDC=$(ssh $SSH_OPTS root@localhost "cat /etc/ssh/sshd_config.d/99-local.conf 2>/dev/null")
if ! echo "$SSHDC" | grep -q 'X11Forwarding yes'; then
    echo "FAIL: sshd_config.d/99-local.conf not preserved (got: $SSHDC)"; exit 1
fi
echo "OK: /etc/ssh/sshd_config.d/99-local.conf preserved."

# Wallpaper file under user home
WP=$(ssh $SSH_OPTS root@localhost "ls -l /var/home/realuser/Pictures/migration-wallpaper.png 2>/dev/null | awk '{print \$5}'")
if [ -z "$WP" ] || [ "$WP" = "0" ]; then
    echo "FAIL: wallpaper file missing or empty under /var/home/realuser/Pictures/"; exit 1
fi
echo "OK: wallpaper file ($WP bytes) preserved under /var/home/realuser/Pictures/."

# GNOME extension dir
EXT_META=$(ssh $SSH_OPTS root@localhost "cat /var/home/realuser/.local/share/gnome-shell/extensions/migration-test@e2e/metadata.json 2>/dev/null")
if ! echo "$EXT_META" | grep -q 'migration-test@e2e'; then
    echo "FAIL: GNOME extension metadata not preserved (got: $EXT_META)"; exit 1
fi
echo "OK: GNOME extension dir + metadata preserved."

# dconf user db sentinel + gsettings keyfile fallback
DCONF=$(ssh $SSH_OPTS root@localhost "cat /var/home/realuser/.config/dconf/user 2>/dev/null")
if [ "$DCONF" != "DCONF-USER-DB-SENTINEL" ]; then
    echo "FAIL: dconf user db not preserved (got: $DCONF)"; exit 1
fi
GSK=$(ssh $SSH_OPTS root@localhost "cat /var/home/realuser/.config/glib-2.0/settings/keyfile 2>/dev/null")
if ! echo "$GSK" | grep -q "accent-color='blue'"; then
    echo "FAIL: gsettings keyfile (accent color) not preserved"; exit 1
fi
if ! echo "$GSK" | grep -q 'migration-wallpaper.png'; then
    echo "FAIL: gsettings keyfile (wallpaper URI) not preserved"; exit 1
fi
if ! echo "$GSK" | grep -q "switch-windows=\['<Alt>Tab'\]"; then
    echo "FAIL: gsettings keyfile (custom keybinding) not preserved"; exit 1
fi
echo "OK: dconf db + gsettings keyfile (accent, wallpaper URI, keybinding) preserved."

# Homebrew prefix
BREW_RECEIPT=$(ssh $SSH_OPTS root@localhost "cat /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/INSTALL_RECEIPT.json 2>/dev/null")
if ! echo "$BREW_RECEIPT" | grep -q '"jq"'; then
    echo "FAIL: Homebrew prefix not preserved (got: $BREW_RECEIPT)"; exit 1
fi
BREW_BIN_EXEC=$(ssh $SSH_OPTS root@localhost "test -x /var/home/linuxbrew/.linuxbrew/Cellar/jq/1.7.1/bin/jq && echo yes || echo no")
if [ "$BREW_BIN_EXEC" != "yes" ]; then
    echo "FAIL: brew formula binary lost executable bit"; exit 1
fi
echo "OK: Homebrew Cellar preserved (INSTALL_RECEIPT.json + executable binary mode)."

# Flatpak user + system stubs
FLAT_USER=$(ssh $SSH_OPTS root@localhost "cat /var/home/realuser/.local/share/flatpak/app/org.gnome.Calculator/current/active/metadata 2>/dev/null")
if [ "$FLAT_USER" != "flatpak-user-stub-org.gnome.Calculator" ]; then
    echo "FAIL: per-user flatpak install not preserved (got: $FLAT_USER)"; exit 1
fi
FLAT_SYS=$(ssh $SSH_OPTS root@localhost "cat /var/lib/flatpak/app/com.example.SystemApp/current/active/metadata 2>/dev/null")
if [ "$FLAT_SYS" != "flatpak-system-stub-com.example.SystemApp" ]; then
    echo "FAIL: system flatpak install not preserved (got: $FLAT_SYS)"; exit 1
fi
echo "OK: Flatpak user + system installations preserved."

# --- undo (migration rollback cleanup) test ---
# Selected with E2E_TEST_MODE=undo. We are booted into composefs after the
# migration; verify `undo` removes the composefs boot path + staged deployment,
# preserves OSTree + user state, and the system falls back to OSTree on reboot.
if [ "$E2E_TEST_MODE" = "undo" ]; then
    step "=== Running undo (migration rollback cleanup) test ==="

    UNDO_DRY=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate undo --dry-run 2>&1" || true)
    echo "$UNDO_DRY" | sed 's/^/  /'
    if ! echo "$UNDO_DRY" | grep -qiE 'Removing|composefs'; then
        echo "FAIL: undo --dry-run listed nothing to remove"; exit 1
    fi
    PRE_CFS=$(ssh $SSH_OPTS root@localhost "ls -d /boot/efi/EFI/Linux/bootc_composefs-* 2>/dev/null | wc -l")
    if [ "$PRE_CFS" -lt 1 ]; then
        echo "FAIL: no composefs ESP artifacts before undo (dry-run may have deleted them)"; exit 1
    fi
    echo "OK: undo --dry-run lists artifacts and changes nothing."

    # Real undo (non-destructive default: keep the composefs object store).
    UNDO_OUT=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate undo 2>&1" || {
        echo "FAIL: undo exited non-zero"; exit 1; })
    echo "$UNDO_OUT" | sed 's/^/  /'

    POST_CFS=$(ssh $SSH_OPTS root@localhost "ls -d /boot/efi/EFI/Linux/bootc_composefs-* 2>/dev/null | wc -l")
    [ "$POST_CFS" -eq 0 ] || { echo "FAIL: composefs ESP artifacts remain after undo ($POST_CFS)"; exit 1; }
    SD_PRESENT=$(ssh $SSH_OPTS root@localhost "test -d /boot/efi/EFI/systemd && echo present || echo absent")
    [ "$SD_PRESENT" = absent ] || { echo "FAIL: systemd-boot still on ESP after undo"; exit 1; }
    echo "OK: undo removed composefs boot artifacts + systemd-boot from ESP."

    # The non-full undo must preserve OSTree so fallback works and a retry is possible.
    OSTREE_REPO=$(ssh $SSH_OPTS root@localhost "test -d /sysroot/ostree/repo && echo yes || echo no")
    [ "$OSTREE_REPO" = yes ] || { echo "FAIL: plain undo removed /sysroot/ostree (should be preserved)"; exit 1; }
    echo "OK: OSTree deployment preserved by undo."

    # Reboot — must come back on OSTree (fallback), not composefs.
    ssh $SSH_OPTS root@localhost "systemctl reboot" || true
    sleep 5
    UNDO_BOOT_OK=no
    for _ in $(seq 1 60); do
        if ssh $SSH_OPTS root@localhost true 2>/dev/null; then UNDO_BOOT_OK=yes; break; fi
        sleep 3
    done
    if [ "$UNDO_BOOT_OK" != yes ]; then
        echo "FAIL: VM did not boot after undo"; tail -100 qemu.log; exit 1
    fi
    UNDO_CMDLINE=$(ssh $SSH_OPTS root@localhost "cat /proc/cmdline")
    if echo "$UNDO_CMDLINE" | grep -q 'composefs='; then
        echo "FAIL: still booting composefs after undo (cmdline: $UNDO_CMDLINE)"; exit 1
    fi
    echo "$UNDO_CMDLINE" | grep -q 'ostree=' || {
        echo "FAIL: post-undo boot is neither composefs nor ostree"; exit 1; }
    echo "OK: system booted OSTree after undo (fallback restored)."

    WP_UNDO=$(ssh $SSH_OPTS root@localhost "ls -l /var/home/realuser/Pictures/migration-wallpaper.png 2>/dev/null | awk '{print \$5}'")
    [ -n "$WP_UNDO" ] && [ "$WP_UNDO" != 0 ] || { echo "FAIL: user data lost after undo ($WP_UNDO)"; exit 1; }
    echo "OK: user data preserved through undo ($WP_UNDO bytes)."

    step "=== E2E TEST PASSED SUCCESSFULY ==="
    exit 0
fi

# --- OSTree rollback test (#22) ---
# Verify the migration isn't one-way: reorder BootOrder to put Fedora\shim
# first (which chains into GRUB → the OSTree BLS entry), confirm Bluefin's
# pre-migration state is still mounted, then restore BootOrder and return to
# composefs. Uses BootOrder (honoured by OVMF) rather than BootNext (ignored).
step "=== Running OSTree rollback test ==="

wait_for_ssh_with_msg() {
    local label="$1"
    local max="$2"
    local start=$SECONDS

    local i=1
    while [ $i -le "$max" ]; do
        if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
            step "$label after $((SECONDS - start))s."
            return 0
        fi
        sleep 3
        i=$((i + 1))
    done
    echo "ERROR: $label timeout ($((SECONDS - start))s)" >&2
    return 1
}

# Capture the Boot#### entry for the Fedora\shim path and Linux Boot Manager.
# awk: strip all non-digits from Boot####* → bare hex number (0007, 0008).
FEDORA_BOOTNUM=$(ssh $SSH_OPTS root@localhost "efibootmgr -v 2>/dev/null | awk '/shimx64.efi/ { gsub(/[^0-9]/, \"\", \$1); print \$1; exit }'")
SDBOOT_BOOTNUM=$(ssh $SSH_OPTS root@localhost "efibootmgr -v 2>/dev/null | awk '/Linux Boot Manager/ { gsub(/[^0-9]/, \"\", \$1); print \$1; exit }'")
if [ -z "$FEDORA_BOOTNUM" ] || [ -z "$SDBOOT_BOOTNUM" ]; then
    echo "FAIL: could not locate shim ($FEDORA_BOOTNUM) or Linux Boot Manager ($SDBOOT_BOOTNUM) in efibootmgr"
    ssh $SSH_OPTS root@localhost "efibootmgr -v" >&2
    exit 1
fi

# Save the original BootOrder (systemd-boot first) so we can restore it
# from the OSTree-booted system before returning to composefs.
ORIG_BOOTORDER=$(ssh $SSH_OPTS root@localhost "efibootmgr 2>/dev/null | awk '/^BootOrder:/ {print \$2}'")
step "rollback: reordering BootOrder to $FEDORA_BOOTNUM,$SDBOOT_BOOTNUM (was $ORIG_BOOTORDER)"

# Put Fedora shim first, sd-boot second. OVMF honours BootOrder.
ssh $SSH_OPTS root@localhost "efibootmgr --bootorder $FEDORA_BOOTNUM,$SDBOOT_BOOTNUM >/dev/null && systemctl reboot" || true

sleep 3
wait_for_ssh_with_msg "OSTree rollback boot SSH" 120 || {
    echo "FAIL: VM did not come back after OSTree rollback boot"
    tail -100 qemu.log
    exit 1
}

# Cmdline must show the *OSTree* path: composefs= absent, ostree= present.
ROLLBACK_CMDLINE=$(ssh $SSH_OPTS root@localhost "cat /proc/cmdline")
if echo "$ROLLBACK_CMDLINE" | grep -q 'composefs='; then
    echo "FAIL: VM booted into composefs instead of OSTree (cmdline: $ROLLBACK_CMDLINE)"
    exit 1
fi
if ! echo "$ROLLBACK_CMDLINE" | grep -q 'ostree='; then
    echo "FAIL: rollback boot has neither composefs= nor ostree= (cmdline: $ROLLBACK_CMDLINE)"; exit 1
fi
echo "OK: OSTree fallback boot — composefs= absent, ostree= present."

# Bluefin's pre-migration state lives at /ostree/deploy/<n>/var/. On the
# OSTree boot, /var binds to that path — the wallpaper fixture must be there.
WP_AFTER_ROLLBACK=$(ssh $SSH_OPTS root@localhost "ls -l /var/home/realuser/Pictures/migration-wallpaper.png 2>/dev/null | awk '{print \$5}'")
if [ -z "$WP_AFTER_ROLLBACK" ] || [ "$WP_AFTER_ROLLBACK" = "0" ]; then
    echo "FAIL: Bluefin /var wallpaper missing after rollback ($WP_AFTER_ROLLBACK bytes)"; exit 1
fi
echo "OK: Bluefin /var preserved through rollback ($WP_AFTER_ROLLBACK bytes)."

# Restore the original BootOrder (systemd-boot first) before rebooting back.
step "rollback: restoring BootOrder to $ORIG_BOOTORDER and returning to composefs"
ssh $SSH_OPTS root@localhost "efibootmgr --bootorder $ORIG_BOOTORDER >/dev/null && systemctl reboot" || true
sleep 3
wait_for_ssh_with_msg "Return-to-composefs SSH" 60 || {
    echo "FAIL: VM did not come back to composefs after rollback"
    tail -100 qemu.log
    exit 1
}

RETURN_CMDLINE=$(ssh $SSH_OPTS root@localhost "cat /proc/cmdline")
if ! echo "$RETURN_CMDLINE" | grep -q 'composefs='; then
    echo "FAIL: did not return to composefs (cmdline: $RETURN_CMDLINE)"; exit 1
fi
echo "OK: Returned to composefs cleanly via restored BootOrder."

# --- commit subcommand cleanup test (#25) ---
# Verify the post-commit on-disk layout is byte-shape identical to a fresh
# bootc install of the target image: /sysroot/ostree removed, OSTree BLS
# entries dropped, GRUB2 bits gone (since we migrated to systemd-boot),
# .bootc-aleph.json gone.
step "=== Running commit cleanup test ==="

# Dry-run first — no changes, but the report must list the paths we expect
# to reclaim.
DRYRUN_OUT=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate commit --dry-run 2>&1" || true)
echo "$DRYRUN_OUT" | sed 's/^/  /'
for needle in '/sysroot/ostree' '/sysroot/.bootc-aleph.json' 'Would reclaim'; do
    if ! echo "$DRYRUN_OUT" | grep -qF "$needle"; then
        echo "FAIL: commit --dry-run did not mention '$needle'"; exit 1
    fi
done
echo "OK: commit --dry-run lists expected cleanup targets."

# Confirm those paths are still there before the real commit.
PRE_OSTREE=$(ssh $SSH_OPTS root@localhost "test -d /sysroot/ostree && echo yes || echo no")
if [ "$PRE_OSTREE" != "yes" ]; then
    echo "FAIL: /sysroot/ostree absent before commit — dry-run should have been a no-op"; exit 1
fi

# Real commit.
COMMIT_OUT=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate commit 2>&1" || {
    echo "FAIL: commit subcommand exited non-zero"; exit 1
})
echo "$COMMIT_OUT" | sed 's/^/  /'
if ! echo "$COMMIT_OUT" | grep -q "Reclaimed:"; then
    echo "FAIL: commit didn't print a Reclaimed summary"; exit 1
fi
echo "OK: commit subcommand ran without error."

# Post-conditions: everything OSTree-shaped that belongs to the SOURCE
# deployment should be gone. /sysroot/ostree itself may legitimately
# remain: commit deliberately preserves ostree/bootc — the target bootc
# installation's own container storage (#84's keep-bootc contract in
# transaction.rs::remove_legacy_ostree_content) — and does not rmdir the
# parent. The old `test -d` absence check contradicted that contract and
# could never pass; first caught when the ubuntu-latest probes brought
# this cell back to life (2026-08-27) after the KVM matrix went dark.
POST_OSTREE=$(ssh $SSH_OPTS root@localhost \
    "if [ ! -d /sysroot/ostree ]; then echo absent; else ls -A /sysroot/ostree | grep -v '^bootc\$' | xargs; fi")
if [ -n "$POST_OSTREE" ] && [ "$POST_OSTREE" != "absent" ]; then
    echo "FAIL: legacy content still under /sysroot/ostree after commit: $POST_OSTREE"; exit 1
fi
echo "OK: no legacy OSTree content under /sysroot/ostree (only bootc/ may remain)."

POST_ALEPH=$(ssh $SSH_OPTS root@localhost "test -e /sysroot/.bootc-aleph.json && echo present || echo absent")
if [ "$POST_ALEPH" != "absent" ]; then
    echo "FAIL: /sysroot/.bootc-aleph.json still present after commit"; exit 1
fi
echo "OK: /sysroot/.bootc-aleph.json removed."

OSTREE_BLS=$(ssh $SSH_OPTS root@localhost "ls /boot/loader/entries/ostree-*.conf 2>/dev/null | wc -l")
if [ "$OSTREE_BLS" -ne 0 ]; then
    echo "FAIL: $OSTREE_BLS OSTree BLS entries still in /boot/loader/entries/"; exit 1
fi
echo "OK: stale OSTree BLS entries removed from /boot."

POST_GRUB=$(ssh $SSH_OPTS root@localhost "test -d /boot/grub2 && echo present || echo absent")
if [ "$POST_GRUB" != "absent" ]; then
    echo "FAIL: /boot/grub2 still present after commit (we migrated to systemd-boot)"; exit 1
fi
echo "OK: /boot/grub2 removed."

# Sanity: composefs is still there and the system can still boot. We don't
# reboot here (tests/run-e2e.sh has already proven the round-trip); just
# confirm the composefs subtree wasn't damaged.
POST_CFS=$(ssh $SSH_OPTS root@localhost "test -d /sysroot/composefs && echo yes || echo no")
if [ "$POST_CFS" != "yes" ]; then
    echo "FAIL: /sysroot/composefs gone after commit — we deleted too much!"; exit 1
fi
echo "OK: /sysroot/composefs intact after commit."

# --- Post-commit deep-clean verification ---
# Confirm the on-disk layout matches a fresh `bootc install` of the target
# image: no residual OSTree units, no Bluefin-specific paths, ESP is
# systemd-boot-only, /etc contains no OSTree-era artifacts.
step "=== Running post-commit deep-clean verification ==="

# 1. No ostree-remount.service enablement (OSTree-specific, should be absent).
OSTREE_REMOUNT=$(ssh $SSH_OPTS root@localhost "test -L /etc/systemd/system/local-fs.target.wants/ostree-remount.service && echo yes || echo no")
if [ "$OSTREE_REMOUNT" != "no" ]; then
    echo "FAIL: ostree-remount.service still enabled in /etc/systemd/system/"; exit 1
fi
echo "OK: no ostree-remount.service enablement."

# 2. No rpm-ostree paths in /etc (rpm-ostree-countme.timer, etc.).
RPMOSTREE_CRUFT=$(ssh $SSH_OPTS root@localhost "find /etc/systemd -name '*rpm-ostree*' 2>/dev/null | wc -l")
if [ "$RPMOSTREE_CRUFT" -ne 0 ]; then
    echo "FAIL: $RPMOSTREE_CRUFT rpm-ostree references still in /etc/systemd/"; exit 1
fi
echo "OK: no rpm-ostree unit references in /etc/systemd/."

# 3. No Bluefin-specific BLS entries on ESP (bootc composefs only).
BLUEFIN_ESP=$(ssh $SSH_OPTS root@localhost "find /boot/efi/loader/entries -name '*bluefin*' -o -name '*ostree*' 2>/dev/null | wc -l")
if [ "$BLUEFIN_ESP" -ne 0 ]; then
    echo "FAIL: $BLUEFIN_ESP Bluefin/OSTree BLS entries still on ESP"; exit 1
fi
echo "OK: ESP loader entries are composefs-only."

# 4. No Fedora shim remains on ESP (we migrated to sd-boot).
FEDORA_EFI=$(ssh $SSH_OPTS root@localhost "test -d /boot/efi/EFI/fedora && echo present || echo absent")
if [ "$FEDORA_EFI" != "absent" ]; then
    echo "FAIL: /boot/efi/EFI/fedora still present after systemd-boot migration"; exit 1
fi
echo "OK: ESP /EFI/fedora removed (migrated to systemd-boot)."

# 5. No OSTree deployment configs in /etc.
OSTREE_ETC=$(ssh $SSH_OPTS root@localhost "test -f /etc/ostree/ostree.repo || test -d /etc/ostree/remotes.d && echo yes || echo no")
if [ "$OSTREE_ETC" != "no" ]; then
    echo "FAIL: /etc/ostree/ configs still present"; exit 1
fi
echo "OK: no /etc/ostree configs."

# 6. bootc status still clean after commit (composefs, no rollback pending).
BOOTC_STATUS_POST=$(ssh $SSH_OPTS root@localhost "bootc status --json 2>/dev/null")
if ! echo "$BOOTC_STATUS_POST" | grep -q '"composefs"'; then
    echo "FAIL: bootc status lost composefs backend after commit"; exit 1
fi
echo "OK: bootc status confirms composefs backend."

# --- Target subscription + bootc update verification ---
# After commit the system must be a normal composefs bootc system subscribed to
# the target image:tag, and able to fetch updates from it.
step "=== Verifying target subscription + bootc update ==="

BOOTC_JSON=$(ssh $SSH_OPTS root@localhost "bootc status --json 2>/dev/null")
BOOTED_IMG=$(echo "$BOOTC_JSON" | jq -r '.status.booted.image.image.image // empty')
echo "Booted image reference: ${BOOTED_IMG:-<none>}"
if [ -z "$BOOTED_IMG" ]; then
    echo "FAIL: bootc status reports no booted image reference"; exit 1
fi
# Must be subscribed to the target repo:tag we migrated to (e.g. dakota:stable).
if ! echo "$BOOTED_IMG" | grep -qF "$TARGET_REPO_TAG"; then
    echo "FAIL: booted image '$BOOTED_IMG' is not subscribed to target '$TARGET_REPO_TAG'"; exit 1
fi
echo "OK: subscribed to target image ($BOOTED_IMG)."

# bootc must be able to reach that registry and check for an update. Exit 0
# means it queried the target successfully (the image is already current, so no
# update is staged — we only assert reachability, not that one exists).
if UPGRADE_OUT=$(ssh $SSH_OPTS root@localhost "bootc upgrade --check 2>&1"); then
    echo "$UPGRADE_OUT" | sed 's/^/  /'
    echo "OK: bootc upgrade --check reached the target registry."
else
    echo "$UPGRADE_OUT" | sed 's/^/  /'
    echo "FAIL: bootc upgrade --check could not query target '$VM_TARGET_IMAGE'"; exit 1
fi

# --- Post-commit diff against fresh Dakota reference ---
# Capture a file listing of the post-commit system and compare against
# a fresh Dakota container image. Saved as a diagnostic artifact — not a
# hard assertion. Differences should be only intentional user state
# (/etc customisations, /var data, /home).
step "=== Capturing post-commit vs fresh-Dakota diff ==="

# File listing from the post-commit VM (key subtrees).
ssh $SSH_OPTS root@localhost "find /etc /boot/loader/entries /boot/efi/loader/entries /sysroot/composefs/images /sysroot/state -type f -o -type l 2>/dev/null | sort" > /tmp/e2e-post-commit-files.txt 2>/dev/null

# File listing from a fresh Dakota container (reference factory state).
# Pull if not cached, then list factory /etc + /usr paths (not /var or /home — those are seeded empty).
podman pull --quiet "$TARGET_IMAGE" 2>/dev/null || true
podman run --rm "$TARGET_IMAGE" find /etc /usr -type f -o -type l 2>/dev/null | sort > /tmp/e2e-fresh-dakota-files.txt 2>/dev/null

# Diff: show paths in post-commit that are NOT in the fresh factory image.
# These should be user-introduced files only.
echo "=== Files present post-commit but absent from fresh Dakota (user state) ===" >> e2e-run.log
comm -23 /tmp/e2e-post-commit-files.txt /tmp/e2e-fresh-dakota-files.txt 2>/dev/null | head -100 | tee -a e2e-run.log || true
echo "=== End diff ===" >> e2e-run.log

# Count lines for summary.
EXTRA_COUNT=$(comm -23 /tmp/e2e-post-commit-files.txt /tmp/e2e-fresh-dakota-files.txt 2>/dev/null | wc -l)
echo "Post-commit diff summary: $EXTRA_COUNT paths present beyond fresh Dakota factory state (expected: user /etc + /var data)." | tee -a e2e-run.log

step "=== E2E TEST PASSED SUCCESSFULY ==="
