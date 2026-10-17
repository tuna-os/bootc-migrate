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

`check_free_space` uses a **1.1× multiplier** when reflink is available (vs 1.5×
without), because reflink shares existing data blocks.

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

The tool detects the filesystem type (`is_btrfs = false`, `fs_type = "xfs"`).
It probes reflink support separately.

### Pre-Phase 1 — ext4 loopback for fs-verity

`setup_composefs_loopback_if_needed()` runs before Phase 1 when `fs_type == "xfs"`:

1. Creates a sparse file at `/sysroot/composefs-loopback.ext4`.  
   Size = `clamp(ceil(ostree_repo_GB × 1.5 + 5), 10, 30)` GB.
2. Formats the file as ext4 with `-O verity` (`mkfs.ext4 -F -O verity`).
3. Mounts the file as a loop device at `/sysroot/composefs` with `-o loop`.

Later composefs actions will use `/sysroot/composefs`. This is an ext4
filesystem in a sparse file on XFS, which bypasses the lack of verity support in XFS.

The loopback setup is idempotent. If the file exists and the host mounts it,
the tool detects the mount with `findmnt` and skips creation.

### Phase 1 — OSTree object import

On XFS without reflink, `check_free_space` uses a **1.5× multiplier** and does
full file copies. This process is slower and uses more space. If the XFS volume
supports reflink, the tool uses reflink clones.

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
| Free-space multiplier     | 1.1×                       | 1.5× (without reflink)           |
| LVM root                  | uncommon                   | typical (Bluefin LTS default)    |
| initrd rebuild needed     | no                         | yes — dracut --add "lvm dm"      |
| dracut on source system   | not present                | present (CentOS Stream 10 base)  |

The tool will support dedicated `/var` partitions on any root filesystem type.
See [Dedicated `/var` volume or Btrfs subvolume](#dedicated-var-volume-or-btrfs-subvolume).
