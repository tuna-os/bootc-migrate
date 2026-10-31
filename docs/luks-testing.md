# LUKS end-to-end testing

The `xfs+crypt` scenario in `tests/run-e2e.sh` (`just e2e-luks`) tests a
LUKS root through the migrate-then-boot pipeline. This note describes the
wiring and known gaps from the reference LUKS test in
[`projectbluefin/dakota-iso`](https://github.com/projectbluefin/dakota-iso)
(`docs/luks-testing.md`).

## How this project's LUKS e2e differs from dakota-iso

| | dakota-iso | this project |
|---|---|---|
| Install method | `fisherman` fresh install inside a **live ISO VM** | `bootc install to-disk` in a **privileged podman container** writing a loop device, then migrate |
| Encryption | `encryption.type = luks-passphrase` | `bootc install --block-setup tpm2-luks` |
| Unlock at boot | passphrase typed via QEMU monitor `sendkey` (`luks-unlock.py`) | TPM2 auto-unlock (no interaction) |
| TPM needed | no | **yes — an emulated TPM (swtpm) must be attached to QEMU** |

Because unlock is automatic (TPM2), this project does not need the
monitor or `sendkey` tools from dakota-iso. It needs a vTPM device instead.

## Current flow (`FILESYSTEM=xfs+crypt`)

1. `bootc install to-disk --block-setup tpm2-luks` uses the **target (Dakota)**
   image as the installer. This creates a `/boot` partition, BLS entries, and a
   LUKS root. `bootc install` injects the SSH key. The script skips the step
   for SSH injection (`SKIP_SETUP=true`).
2. The test starts an emulated TPM (swtpm). It attaches swtpm to QEMU with
   `SWTPM_QEMU_ARGS` so root unlocks at boot.
3. QEMU boots the migrated disk; the test waits for SSH and runs the in-VM and
   host-side validations.

### swtpm wiring

`run-e2e.sh` launches swtpm only for `xfs+crypt`:

```
swtpm socket --tpm2 --tpmstate dir=/tmp/swtpm-tpmstate \
  --ctrl type=unixio,path=/tmp/swtpm-sock --daemon --pid file=/tmp/swtpm.pid
# QEMU: -chardev socket,id=chrtpm,path=/tmp/swtpm-sock \
#       -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-crb,tpmdev=tpm0
```

`swtpm` + `swtpm-tools` are installed in the e2e CI job. Locally, install via
your distro (`dnf install swtpm swtpm-tools` / `apt install swtpm swtpm-tools`).

## Root cause: install-TPM vs boot-TPM mismatch

`--block-setup tpm2-luks` enrolls the LUKS key against the TPM2 present
**during install**. Now, that is inside the podman container, which uses the host
TPM or none at all. The VM then boots with a **different** TPM (the swtpm). The
VM cannot unseal the key at boot. An swtpm instance is necessary for TPM2
unlock, but it is not enough alone. The install TPM and the boot TPM must be the
same device.

## Chosen direction: fisherman's `bootc install to-filesystem` recipe

Instead of `bootc install to-disk --block-setup tpm2-luks` (which delegates LUKS
to bootc and ties enrollment to the install-time TPM), use the
[`projectbluefin/fisherman`](https://github.com/projectbluefin/fisherman)
process: **set up LUKS yourself, then `bootc install to-filesystem` into the
already-opened mapper.** bootc then only sees `/dev/mapper/root` and writes
`root=UUID=<fs-uuid>` with no LUKS parameters — you own the unlock story.

### The bug in the old code

Host-side LUKS code before commit `4d21116` set `rd.luks.name=$LUKS_MAPPER`
with a bare mapper name. That format is invalid. `rd.luks.name` takes
`<UUID>=<name>`. Fisherman uses `rd.luks.name=<luksUUID>=root`, which maps the
container to `/dev/mapper/root`. Without this form, the initrd cannot find the
root partition. It hangs for 90 seconds before an emergency shell
(projectbluefin/dakota#270).

### What we actually test: GRUB source → migrate to systemd-boot

Bluefin and Bluefin-LTS use **GRUB**. The e2e test must install the source with
GRUB and LUKS. Then `bootc-migrate` converts the system to systemd-boot with
composefs. Direct install of Dakota does not exercise the tool.

So the source install uses fisherman's **`DiskLayoutGrub`** — three partitions,
with a **separate unencrypted ext4 `/boot`**:

| Part | Size | FS | Why |
|------|------|----|-----|
| p1 EFI System | 512 MiB | FAT32 | UEFI bootloader |
| p2 `/boot` | 1 GiB | **ext4** | GRUB reads kernel/initrd here without parsing the LUKS/xfs root; also lets `bootupctl`'s bwrap sandbox find the boot-fs UUID |
| p3 root | rest | LUKS2 → xfs | encrypted root |

```sh
# 1. Partition (GRUB layout: ESP + ext4 /boot + LUKS root)
sgdisk --zap-all "$DISK"
sgdisk -n 1:0:+512MiB -t 1:ef00 -c 1:EFI-SYSTEM \
       -n 2:0:+1GiB   -t 2:8300 -c 2:boot \
       -n 3:0:0       -t 3:8300 -c 3:root "$DISK"
ESP=${DISK}p1; BOOT=${DISK}p2; ROOT=${DISK}p3

# 2. LUKS2 on root (e2e uses a keyfile for deterministic unlock)
cryptsetup luksFormat --batch-mode --type luks2 --key-file "$KEY" "$ROOT"
cryptsetup luksOpen --key-file "$KEY" "$ROOT" root        # -> /dev/mapper/root
LUKS_UUID=$(cryptsetup luksUUID "$ROOT")

# 3. Format + mount root, /boot, and ESP
mkfs.xfs  -f /dev/mapper/root
mkfs.ext4 -F "$BOOT"
mkfs.vfat -F32 "$ESP"
mount /dev/mapper/root /mnt/target
mkdir -p /mnt/target/boot       && mount "$BOOT" /mnt/target/boot
mkdir -p /mnt/target/boot/efi   && mount "$ESP"  /mnt/target/boot/efi

# 4. Install the BLUEFIN SOURCE (OSTree/GRUB — no --composefs-backend)
podman run --privileged --pid=host -v /dev:/dev -v /mnt/target:/mnt/target \
  "$INSTALL_IMAGE" bootc install to-filesystem --generic-image \
  --root-ssh-authorized-keys /workspace/test_key.pub /mnt/target

# 5. Auto-unlock + the CRITICAL BLS arg form (GRUB entries live on ext4 /boot)
mkdir -p /mnt/target/boot/keys && cp "$KEY" /mnt/target/boot/keys/luks.key
sed -i "s|^\(options .*\)|\1 rd.luks.name=$LUKS_UUID=root rd.luks.key=/keys/luks.key|" \
    /mnt/target/boot/loader/entries/*.conf

# 6. Tear down, then boot + run the migration as the non-LUKS path already does
umount /mnt/target/boot/efi /mnt/target/boot /mnt/target && cryptsetup luksClose root
```

After boot, `bootc-migrate` migrates to systemd-boot and composefs. The
migration must copy the `rd.luks.*` args to the new systemd-boot BLS entries on
the ESP. Without them, the next boot loses LUKS unlock. An e2e assertion must test
this requirement for encrypted system migrations.

For the production path, replace the keyfile with fisherman's TPM2 enrollment —
`systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 --unlock-key-file=<key>`
(PCR 7 = Secure Boot state, stable across boots) and keep a passphrase fallback.
That step needs a real TPM, so it must run in the VM (with the swtpm wired
above), not in the podman install container.

### Status / remaining work

- ✅ swtpm launched + wired into QEMU (`SWTPM_QEMU_ARGS`) with cleanup; CI installs
  `swtpm`/`swtpm-tools`. Prerequisite for the TPM2 path.
- ✅ Root-caused the boot failure: malformed `rd.luks.name`, missing vTPM, and the
  install/boot TPM mismatch.
- ⏳ Rewrite the `xfs+crypt` install block with the recipe above instead of
  `--block-setup tpm2-luks`.
- Validate changes across QEMU boots. Keep `fail-fast: false` until the test passes.

## Lessons carried over from dakota-iso

- **Keep disk images and OVMF VARS outside `/tmp`.** `/tmp` often uses a small
  tmpfs. Large images fill it and the VM fails. Keep `disk.raw` in the
  workspace directory.
- **Check boot and SSH status on a short interval** (seconds, not minutes).
  This reports failures fast before the job timeout.
- **Always record the serial log** (`qemu.log`) and upload it on failure.
  Failures to unlock LUKS cause a silent hang on the console.
