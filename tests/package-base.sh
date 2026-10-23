#!/bin/bash
# Prepare a package-managed disk image for tests/run-e2e.sh.
#
# The inputs are deliberately environment-driven so the matrix can select a
# real distribution image without teaching the main harness about download
# URLs, checksums, or partition surgery.

set -euo pipefail

PACKAGE_BASE="${E2E_PACKAGE_BASE:?E2E_PACKAGE_BASE is required}"
DISK_PATH="${E2E_PACKAGE_DISK:?E2E_PACKAGE_DISK is required}"
SEED_PATH="${E2E_PACKAGE_SEED:?E2E_PACKAGE_SEED is required}"
DISK_SIZE="${E2E_PACKAGE_DISK_SIZE:?E2E_PACKAGE_DISK_SIZE is required}"
SSH_PUBLIC_KEY="${E2E_SSH_PUBLIC_KEY:?E2E_SSH_PUBLIC_KEY is required}"
CACHE_DIR="${E2E_PACKAGE_CACHE:-$(dirname "$DISK_PATH")/.e2e-package-images}"

FEDORA_CLOUD_URL="https://download.fedoraproject.org/pub/fedora/linux/releases/43/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2"
FEDORA_CLOUD_SHA256="846574c8a97cd2d8dc1f231062d73107cc85cbbbda56335e264a46e3a6c8ab2f"
FEDORA_CLOUD_NAME="Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2"
UBUNTU_CLOUD_URL="https://cloud-images.ubuntu.com/releases/noble/release-20261017/ubuntu-24.04-server-cloudimg-amd64.img"
UBUNTU_CLOUD_SHA256="04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94"
UBUNTU_CLOUD_NAME="ubuntu-24.04-server-cloudimg-amd64.img"

case "$PACKAGE_BASE" in
    fedora-cloud|fedora-workstation)
        SOURCE_URL="${E2E_PACKAGE_BASE_URL:-$FEDORA_CLOUD_URL}"
        SOURCE_SHA256="${E2E_PACKAGE_BASE_SHA256:-$FEDORA_CLOUD_SHA256}"
        SOURCE_NAME="${SOURCE_URL##*/}"
        [ -n "$SOURCE_NAME" ] || SOURCE_NAME="$FEDORA_CLOUD_NAME"
        ;;
    ubuntu-cloud)
        SOURCE_URL="${E2E_PACKAGE_BASE_URL:-$UBUNTU_CLOUD_URL}"
        SOURCE_SHA256="${E2E_PACKAGE_BASE_SHA256:-$UBUNTU_CLOUD_SHA256}"
        SOURCE_NAME="${SOURCE_URL##*/}"
        [ -n "$SOURCE_NAME" ] || SOURCE_NAME="$UBUNTU_CLOUD_NAME"
        ;;
    *)
        echo "ERROR: unknown E2E_PACKAGE_BASE '$PACKAGE_BASE' (fedora-cloud, fedora-workstation, ubuntu-cloud)" >&2
        exit 2
        ;;
esac

mkdir -p "$CACHE_DIR"
SOURCE_PATH="$CACHE_DIR/$SOURCE_NAME"

if [ ! -s "$SOURCE_PATH" ]; then
    echo "Downloading package base image: $SOURCE_URL"
    curl --fail --location --retry 5 --retry-delay 2 --output "$SOURCE_PATH.part" "$SOURCE_URL"
    mv "$SOURCE_PATH.part" "$SOURCE_PATH"
fi
printf '%s  %s\n' "$SOURCE_SHA256" "$SOURCE_PATH" | sha256sum -c -

if ! command -v qemu-img >/dev/null 2>&1; then
    echo "ERROR: qemu-img is required for package-based E2E images" >&2
    exit 1
fi
if ! command -v growpart >/dev/null 2>&1; then
    echo "ERROR: growpart (cloud-guest-utils) is required for package-based E2E images" >&2
    exit 1
fi
if ! command -v cloud-localds >/dev/null 2>&1; then
    echo "ERROR: cloud-localds (cloud-image-utils) is required for package-based E2E images" >&2
    exit 1
fi

USER_DATA="$(mktemp)"
META_DATA="$(mktemp)"
cleanup_seed() {
    rm -f "$USER_DATA" "$META_DATA"
}
trap cleanup_seed EXIT

# Cloud-init is present in both official cloud images. It supplies the first
# SSH key and the e2e socket before the harness's live fixture step runs.
cat > "$USER_DATA" <<EOF
#cloud-config
preserve_hostname: false
hostname: e2e-package-base
disable_root: false
ssh_pwauth: false
users:
  - name: root
    lock_passwd: false
    ssh_authorized_keys:
      - $SSH_PUBLIC_KEY
ssh_authorized_keys:
  - $SSH_PUBLIC_KEY
write_files:
  - path: /etc/ssh/sshd_config.d/90-e2e-root.conf
    permissions: '0600'
    content: |
      PermitRootLogin yes
      PasswordAuthentication no
  - path: /etc/systemd/system/e2e-sshd.socket
    permissions: '0644'
    content: |
      [Unit]
      Description=E2E SSH TCP Socket (port 22)
      [Socket]
      ListenStream=22
      Accept=yes
      [Install]
      WantedBy=sockets.target
  - path: /etc/systemd/system/e2e-sshd@.service
    permissions: '0644'
    content: |
      [Unit]
      Description=E2E SSH per-connection service
      [Service]
      ExecStart=-/usr/sbin/sshd -i
      StandardInput=socket
packages:
  - openssh-server
  - podman
  - NetworkManager
runcmd:
  - [bash, -c, 'systemctl disable --now sshd.service || systemctl disable --now ssh.service || true']
  - [bash, -c, 'systemctl enable --now e2e-sshd.socket']
EOF

if [ "$PACKAGE_BASE" = fedora-workstation ]; then
    cat >> "$USER_DATA" <<'EOF'
  - [bash, -c, 'dnf -y group install "Fedora Workstation" || dnf -y install @workstation-product-environment']
  - [bash, -c, 'systemctl set-default graphical.target']
  - [bash, -c, 'systemctl enable --now gdm.service']
EOF
fi

cat > "$META_DATA" <<EOF
instance-id: e2e-$PACKAGE_BASE
local-hostname: e2e-package-base
EOF
cloud-localds "$SEED_PATH" "$USER_DATA" "$META_DATA"

rm -f "$DISK_PATH"
echo "Converting package base to raw disk: $DISK_PATH"
qemu-img convert -p -O raw "$SOURCE_PATH" "$DISK_PATH"
qemu-img resize "$DISK_PATH" "$DISK_SIZE"

LOOP_DEV=""
ROOT_PART=""
ROOT_FS=""
ROOT_SIZE=0
MNT_DIR=""
BOOT_PART=""
BOOT_TMP=""
cleanup_disk() {
    set +e
    if [ -n "$MNT_DIR" ] && [ -n "$BOOT_PART" ] && mountpoint -q "$MNT_DIR/boot"; then
        umount "$MNT_DIR/boot"
    fi
    if [ -n "$MNT_DIR" ] && mountpoint -q "$MNT_DIR"; then
        umount "$MNT_DIR"
    fi
    if [ -n "$BOOT_TMP" ]; then
        rm -rf "$BOOT_TMP"
    fi
    if [ -n "$LOOP_DEV" ]; then
        losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
    rmdir "$MNT_DIR" 2>/dev/null || true
}
cleanup_all() {
    cleanup_disk
    cleanup_seed
}
trap cleanup_all EXIT

LOOP_DEV="$(losetup --show -f -P "$DISK_PATH")"
partprobe "$LOOP_DEV" 2>/dev/null || true
udevadm settle 2>/dev/null || true

for part in "$LOOP_DEV"p*; do
    [ -b "$part" ] || continue
    fs="$(blkid -o value -s TYPE "$part" 2>/dev/null || true)"
    case "$fs" in
        btrfs|ext4|xfs)
            size="$(blockdev --getsize64 "$part")"
            if [ "$size" -gt "$ROOT_SIZE" ]; then
                ROOT_PART="$part"
                ROOT_FS="$fs"
                ROOT_SIZE="$size"
            fi
            ;;
    esac
done
[ -n "$ROOT_PART" ] || {
    echo "ERROR: no btrfs, ext4, or xfs root partition found in $DISK_PATH" >&2
    exit 1
}
ROOT_NUM="${ROOT_PART#${LOOP_DEV}p}"

# Keep 1 GiB at the end for the Workstation cell's separate /boot. The cloud
# and Ubuntu cells simply grow their existing root partition to the disk size.
if [ "$PACKAGE_BASE" = fedora-workstation ]; then
    if ! command -v sgdisk >/dev/null 2>&1; then
        echo "ERROR: sgdisk (gdisk) is required for the Fedora Workstation layout" >&2
        exit 1
    fi
    sgdisk --move-second-header "$DISK_PATH" >/dev/null
    # Place /boot at the end so the existing root partition can grow into the
    # resized image before the new partition is created.
    sgdisk --new=0:-1G:0 --typecode=0:8300 "$DISK_PATH" >/dev/null
    udevadm settle 2>/dev/null || true
    growpart "$LOOP_DEV" "$ROOT_NUM"
    partprobe "$LOOP_DEV" 2>/dev/null || true
    udevadm settle 2>/dev/null || true
    BOOT_PART="$(lsblk -lnpo NAME "$LOOP_DEV" | tail -1)"
    [ "$BOOT_PART" != "$ROOT_PART" ] && [ -b "$BOOT_PART" ] || {
        echo "ERROR: could not find the new Fedora Workstation /boot partition" >&2
        exit 1
    }
    mkfs.ext4 -F "$BOOT_PART" >/dev/null
else
    growpart "$LOOP_DEV" "$ROOT_NUM"
    partprobe "$LOOP_DEV" 2>/dev/null || true
    udevadm settle 2>/dev/null || true
fi

MNT_DIR="$(mktemp -d /tmp/e2e-package-root.XXXXXX)"
if [ "$ROOT_FS" = btrfs ]; then
    mount -o subvol=root "$ROOT_PART" "$MNT_DIR" 2>/dev/null || mount "$ROOT_PART" "$MNT_DIR"
else
    mount "$ROOT_PART" "$MNT_DIR"
fi
if [ "$ROOT_FS" = btrfs ]; then
    for subvolume in root home; do
        if ! btrfs subvolume list "$MNT_DIR" | grep -Eq " path $subvolume$"; then
            echo "ERROR: Fedora package base is missing the expected btrfs '$subvolume' subvolume" >&2
            exit 1
        fi
    done
fi

case "$ROOT_FS" in
    btrfs) btrfs filesystem resize max "$MNT_DIR" >/dev/null ;;
    ext4) resize2fs "$ROOT_PART" >/dev/null ;;
    xfs) xfs_growfs "$MNT_DIR" >/dev/null ;;
esac

if [ "$PACKAGE_BASE" = fedora-workstation ]; then
    BOOT_TMP="$(mktemp -d /tmp/e2e-package-boot.XXXXXX)"
    cp -a "$MNT_DIR/boot/." "$BOOT_TMP/"
    mount "$BOOT_PART" "$MNT_DIR/boot"
    cp -a "$BOOT_TMP/." "$MNT_DIR/boot/"
    BOOT_UUID="$(blkid -o value -s UUID "$BOOT_PART")"
    if ! grep -qE '[[:space:]]/boot[[:space:]]' "$MNT_DIR/etc/fstab"; then
        printf 'UUID=%s /boot ext4 defaults 0 2\n' "$BOOT_UUID" >> "$MNT_DIR/etc/fstab"
    fi
    umount "$MNT_DIR/boot"
    rm -rf "$BOOT_TMP"
    BOOT_TMP=""
    echo "Prepared Fedora Workstation layout: btrfs root/home + separate ext4 /boot"
fi

sync
umount "$MNT_DIR"
rmdir "$MNT_DIR"
MNT_DIR=""
losetup -d "$LOOP_DEV"
LOOP_DEV=""
trap - EXIT
cleanup_seed
trap - EXIT

echo "Package base prepared: $PACKAGE_BASE"
echo "  disk: $DISK_PATH"
echo "  seed: $SEED_PATH"
