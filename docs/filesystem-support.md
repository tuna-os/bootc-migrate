# Filesystem Support: btrfs vs XFS

This document explains how the tool supports two root filesystems:
btrfs (Bluefin stable) and XFS (Bluefin LTS). It describes where the two
paths diverge.

## Background: what composefs needs

The composefs object store (`/sysroot/composefs/`) holds content-addressed EROFS
images and data files. When `bootc` pulls an OCI image into this
store, it seals each file with **fs-verity**. This kernel feature calculates
a Merkle-tree hash over file contents to detect corruption.

**fs-verity needs kernel and filesystem support.** As of Linux 6.12:

| Filesystem | fs-verity | reflink (CoW copy) |
|------------|-----------|--------------------|
| btrfs      | ✅ yes    | ✅ yes             |
| ext4       | ✅ yes    | ❌ no              |
| XFS        | ❌ no     | ✅ yes (on some)   |

Bluefin **stable** (Fedora-based) ships btrfs by default.  
Bluefin **LTS** (CentOS Stream 10-based) ships XFS by default.

---

### Post-seal integrity verification

The native composefs-rs backend runs structural `fsck` on the store after the
composefs seal completes. Both backends then read and verify every object
in `/sysroot/composefs/objects`. This read checks data, not only the stored
fs-verity descriptor. It makes storage corruption fail during migration.
A failure stops migration before it stages a boot entry. The check does not
wait for first boot.

## btrfs path (Bluefin stable → Dakota stable)

The standard path needs no workarounds.

### Phase 0 — preflight

`preflight.rs` reads `/proc/mounts` to confirm the root filesystem type. It
probes for reflink support with a copy-on-write clone under `/sysroot`.
On btrfs, both checks pass.

### Phase 1 — OSTree object import (optional)

The tool will clone OSTree files with reflink from `/sysroot/ostree/repo`
into `/sysroot/composefs/objects`. Reflink is fast and uses no extra disk
space on btrfs.

The Phase 0 free-space estimate counts the source repo at **1.1×** when
reflink is available (vs 1.5× without). It also counts space for two expanded
copies of the target image (ComposeFS and podman fallback storage) and 5 GiB of
temporary initrd-rebuild headroom.

Each imported object uses its SHA-512 fs-verity digest as its name. The import
enables fs-verity before it pulls the target image. Native and legacy store
writers then see the same verified object layout.

### Phase 3 — EROFS seal

`bootc internals cfs seal` calls `ioctl(FS_IOC_ENABLE_VERITY)` on each object file.
btrfs handles this natively without extra setup.

### /var migration

On btrfs, `/var` is typically a subvolume mounted with `subvol=/` or a named
subvolume. The tool copies `/var` data into `/sysroot/state/os/default/var`
(the composefs state path) with `copy_dir_all_with_xattrs`. The composefs
initramfs bind-mounts this path at `/var` after the switch root to preserve user data.

---

## XFS path (Bluefin LTS → Dakota stable)

XFS lacks fs-verity support, so the tool uses a workaround before it seals
composefs objects. Also, Bluefin LTS uses LVM for the root volume, which needs
initrd updates before composefs boots.

### Phase 0 — preflight

Preflight detects the same filesystem type: `is_btrfs = false`,
`fs_type = "xfs"`. It probes reflink separately because not all XFS
configurations support it. The free-space check fetches the compressed layer
size of the target image. It does this before any image pull or storage setup.
It adds space for both expanded target copies and for the initrd rebuild. This
catches a tight XFS root before the sparse loopback and podman storage use it.

### Pre-Phase 1 — ext4 loopback for fs-verity

`prepare_composefs_storage()` runs before Phase 1 when `fs_type == "xfs"`:

1. Creates a sparse file at `/sysroot/composefs-loopback.ext4`.
   Its nominal size is `ceil(ostree_repo_GB × 1.5 + 25)` GB, with a 30 GB
   floor and a cap at 90% of the now-free space.
2. Formats it as ext4 with `-O verity` (`mkfs.ext4 -F -O verity`).
3. Loop-mounts it at `/sysroot/composefs` with `-o loop`.

Later composefs actions will use `/sysroot/composefs`. This is an ext4
filesystem in a sparse file on XFS, which bypasses the lack of verity support in XFS.

The loopback setup is idempotent. If the file exists and the host mounts it,
the tool detects the mount with `findmnt` and skips creation.

### Phase 1 — OSTree object import

On XFS without reflink, the source-repo part of the estimate uses a **1.5×
multiplier**. Phase 1 makes full copies, not reflink clones. This is slower
and uses more space. If reflink is available on the XFS volume, the tool uses
it (the probe is runtime, not hardcoded to XFS=no-reflink).

### Phase 3 — EROFS seal

`bootc internals cfs seal` runs against the composefs store on the ext4 loopback,
where `ioctl(FS_IOC_ENABLE_VERITY)` succeeds.

### Phase 5 — LVM initrd rebuild

Bluefin LTS installs root on LVM (`/dev/mapper/<vg>-<lv>`). The Dakota initrd
from the OCI registry has no LVM modules. It cannot activate the volume group
at boot and enters an emergency shell.

`phase5_setup_bootloader` detects LVM and rebuilds the initrd:

1. **`detect_lvm()`** — checks `/dev/mapper` for entries other than `control`.
   If none exist, the tool skips the rebuild.

2. **`rebuild_initrd_with_lvm_if_needed(kver, mount_path, initrd_dst)`**:
   - Locates `dracut` on the host (`/usr/bin/dracut` or `/usr/sbin/dracut`).
     Bluefin LTS ships dracut; Bluefin stable does not need this step.
   - The Dakota kernel modules are available at
     `<composefs_mount>/usr/lib/modules/<kver>/` through the composefs overlay mount.
   - Creates a temporary symlink: `/lib/modules/<kver>` → composefs mount path.
     On Fedora and CentOS, `/lib` links to `/usr/lib`.
   - Runs: `dracut --kver <kver> --add "lvm dm" --force <initrd_dst>`
   - Removes the symlink in all cases (even if dracut fails).

3. The LVM-enabled initrd replaces the downloaded initrd in place. Because this
   runs before `patch_origin_boot_digest`, the hash in `.origin` covers the final bytes.

If dracut is unavailable or fails, the tool displays a warning with the
command to run. The migration completes, and the user can fix the initrd from
the OSTree fallback entry.

---

## Dedicated `/var` volume or Btrfs subvolume

Standard Anaconda installs often place `/var` on a separate filesystem (or
logical volume), or in a subvolume like `subvol=/var`. Both layouts are
important: composefs must expose existing data at `/var`.

### The problem

The composefs boot mounts `/sysroot/state/os/default/var` onto `/var` and
ignores `/etc/fstab` entries for `/var`. On a composefs boot, a dedicated
`/var` volume remains unmounted. `/var` falls back to an empty directory, so
user homes and flatpaks disappear. The data remains safe on disk, but the
system is unusable.

Two automated steps will fix this issue:

### 1. Activate every LV backing a mounted filesystem

The command line on the source OSTree system usually lists only the root LV
(`rd.lvm.lv=<vg>/root`). Non-root volumes activate after the switch root on
the source distribution. The composefs target may lack this auto-activation step.

`get_kernel_options` finds every LV that supports a mount (`findmnt` → `lvs`).
It adds `rd.lvm.lv=<vg>/<lv>` for each volume, so the initrd activates them
before mounts run.

### 2. Mount the dedicated `/var` at the stateroot var path

Activation alone is insufficient because bootc still binds the stateroot var
onto `/var`. `phase5_setup_bootloader` does these steps:

1. **`detect_separate_var()`** — uses
   `findmnt -o TARGET,SOURCE,FSTYPE,FSROOT,OPTIONS /var`. It accepts either a
   whole filesystem (`FSROOT == "/"`) or a direct Btrfs subvolume.
   The tool will reject arbitrary subtrees. It returns the UUID, filesystem
   type, and mount options.

2. The generated BLS entry adds an initrd option:
   `rd.systemd.mount-extra=...:/sysroot/state/os/default/var:...` with
   dependencies on `bootc-root-setup.service`. This mounts the filesystem at the
   composefs stateroot before bootc prepares the deployment.

   BLS arguments persist across updates: `bootc upgrade` copies current
   arguments to new deployments. Units placed only in the initrd would disappear on upgrade.

3. If the tool rebuilds the initrd for LVM or XFS, it also injects the
   `sysroot-state-os-default-var.mount` unit for compatibility.

`bootc-root-setup` then binds the target path onto `/var`, so user data appears at `/var`.

For a direct Btrfs `subvol=/var` layout, the tool reuses the subvolume in place.
It does not move data to subvolume ID 5.

The `xfs+lvm+crypt` e2e test exercises this path (LVM-on-LUKS with separate
`root` and `var` LVs). Its assertions verify that data on the dedicated volume survives.

## OSTree `/var/home` to native `/home`

OSTree systems expose `/home` as a symlink to `/var/home`. A ComposeFS target
can ship separate `/home` and `/var/home` directories. If the system mounts a home
subvolume at `/home`, symlink paths to `/var/home` become broken.

Phase 4 detects this transition and adds a bind mount to `/etc/fstab`:

- If fstab mounts a volume at `/home`, `/home` is canonical and binds to `/var/home`.
- Otherwise home data stays in `/var/home`, which binds to `/home`.

The tool does not rewrite home files or symlinks. Both paths resolve to the same
data, and the fstab entry persists across `bootc upgrade` deployments.

## Package-based E2E source layouts

The distribution disk determines the source filesystem in package-install E2E
mode. `bootc install` does not select it. `tests/package-base.sh` verifies the
official image checksum and grows the root partition to `DISK_SIZE`. It mounts
the partition only to prepare the layout and cloud-init seed.

| Source cell | Root and home | Boot layout | Package database |
|---|---|---|---|
| Fedora Cloud | btrfs `root` and `home` subvolumes | image's UEFI layout | rpm/dnf |
| Fedora Workstation | btrfs `root` and `home` subvolumes | separate ext4 `/boot` added by the helper | rpm/dnf |
| Ubuntu Cloud | ext4 | image's UEFI layout | dpkg/apt |

The Workstation source uses the Fedora Cloud disk as its reproducible seed.
It then installs the Workstation environment through `dnf`. This tests a
package-managed GNOME/GDM system. It uses the same btrfs layout for home and a
separate `/boot`. Thus it does not test another bootc image. The Ubuntu cell is
ext4/dpkg and passes `--accept-cross-base` explicitly when
moving to `fedora-bootc`.

---

## Re-running after a failed composefs boot

If the first migration try created a non-LVM initrd, the system stops in a
dracut shell. Reboot to the OSTree fallback entry and run:

```bash
bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable \
  --force --skip-import
```

- `--skip-import` reuses existing composefs objects from Phase 1.
- `--force` bypasses the Phase 5 check: the tool removes existing `bootc_*`
  BLS entries and runs Phase 5 again.

---

## Summary table

| Concern                   | btrfs (stable)             | XFS (LTS)                        |
|---------------------------|----------------------------|----------------------------------|
| fs-verity support         | native                     | ext4 loopback at /sysroot/composefs |
| composefs store location  | /sysroot/composefs (btrfs) | /sysroot/composefs (ext4 loop)   |
| Phase 1 object copy       | reflink (instant, ~0 extra space) | full copy or XFS reflink    |
| Free-space estimate       | source repo 1.1× + target copies + 5 GiB | source repo 1.1×/1.5× + target copies + 5 GiB |
| LVM root                  | uncommon                   | typical (Bluefin LTS default)    |
| initrd rebuild needed     | no                         | yes — dracut --add "lvm dm"      |
| dracut on source system   | not present                | present (CentOS Stream 10 base)  |

The tool will support dedicated `/var` partitions on any root filesystem type.
See [Dedicated `/var` volume or Btrfs subvolume](#dedicated-var-volume-or-btrfs-subvolume).
