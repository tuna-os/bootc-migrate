# Architecture Decisions and Lessons Learned

## Overview

This document provides architectural decisions, workarounds, and lessons
learned during the development of the OSTree to composefs migration tool.
It covers both the migration binary (`src/`) and the E2E test harness
(`tests/run-e2e.sh`).

---

## 1. Boot Artifact Extraction

### Problem

Phase 5 must copy the kernel, initrd, and systemd-boot binaries from the
target container image (Dakota) to the ESP or `/boot`. The overlay mount of
composefs EROFS returns zero-filled file content past an inline data threshold
(~4 KB per inode). This can corrupt large files such as vmlinuz (17 MB) and initrd.

### Tested solutions (in order)

| Approach | Peak disk | Time | Result |
|----------|-----------|------|--------|
| bootc cfs overlay mount | 0 | instant | Returns zeros for large files |
| podman cp | 6 GB (full image) | 30s | Works but causes ENOSPC with 14 GB loopback |
| skopeo copy | 6 GB+ | 60s | Same ENOSPC problem |
| Registry stream | 500 MB | ~10 min | ✅ **Final** — downloads layers one at a time |

### Architecture decision: Registry stream

`extract_files_via_registry()` and `extract_subtree_via_registry()` download
OCI layers in steps: fetch → extract needed files → delete blob → repeat.
This limits disk use to the size of the largest single layer (~200 MB).

Used for: vmlinuz, initrd, `systemd-bootx64.efi`, and kernel modules (`/usr/lib/modules/<kver>/`).

### Lesson learned

A bare mount of EROFS does fill content with zeros past the 4 KB inline threshold.
Tools that read large files from a composefs EROFS mount must use the `bootc internals
cfs oci mount` overlay mount or stream data from the registry.

---

## 2. Kernel Module Extraction for Initrd Rebuild

### Problem

An XFS root needs `xfs.ko` in the initrd. The EROFS mount cannot provide
kernel modules because it returns zero-filled files. `podman cp` pulls the full target image
(~6 GB) into podman storage. This causes ENOSPC on the 20 GB disk when the system
allocates the 14 GB loopback.

### Tested solutions

| Approach | Peak disk | Disk needed | Free on XFS | Result |
|----------|-----------|-------------|-------------|--------|
| EROFS mount path | 0 | 0 | 3 GB | Zero-filled |
| podman cp | 6 GB | 6 GB | 3 GB | ENOSPC ❌ |
| containers-storage: | 0 | 0 | 3 GB | Image not in podman |
| Registry stream | 500 MB | 1 GB | 3 GB | ✅ |

### Architecture decision: Registry stream for kernel modules

`extract_subtree_via_registry(target_image, "usr/lib/modules/<kver>", tempdir)`
streams the subtree of kernel modules layer by layer. Peak usage is the largest single layer
(~200 MB) plus the extracted tree (~300 MB) plus the new initrd (~80 MB) (about 580 MB total).

We lowered the free-space check from 6 GB to 1.5 GB.

### Lesson learned

`podman cp` needs to pull the full image into podman storage first.
The `containers-storage:` transport needs the image to be present already.
Neither works when disk space is low. Layer extraction from the registry is the only viable path.

---

## 3. Initrd Rebuild with Bootc Dracut Module

### Problem

The initrd must include `bootc-root-setup.service` to mount the composefs
EROFS as the root filesystem. Without it, the system boots the OSTree
deployment (Bluefin) instead of the composefs overlay (Dakota).

### Discovery

The bootc dracut module (`51bootc/module-setup.sh`) has:
```bash
check() { return 255; }
```

The code `return 255` means dracut **never includes the module automatically** — it needs
explicit `dracut --add bootc`. Our initrd rebuild used plain
`dracut --force --kmoddir` without `--add bootc`.

### Fix

Add `--add bootc` to the dracut command for all initrd rebuilds:
```rust
let dracut_add = format!("{} bootc", mods_str);
cmd.arg("--add").arg(&dracut_add);
```

### Where the bootc dracut module lives

- Module: `/usr/lib/dracut/modules.d/51bootc/module-setup.sh`
- Service: `/usr/lib/systemd/system/bootc-root-setup.service`
- Binary: `/usr/lib/bootc/initramfs-setup` (1.3 MB Rust binary)
- Config: `/usr/lib/composefs/setup-root-conf.toml` (optional)

The dracut module installs these files into the initramfs through its `install()`
function. The bootc module is opt-in because `check() { return 255; }` means
dracut never includes it automatically. You must pass `--add bootc`.

### EROFS mount order in initrd

`bootc-root-setup.service` has:
```ini
ConditionKernelCommandLine=composefs
After=sysroot.mount
After=ostree-prepare-root.service
Before=initrd-root-fs.target
```

The system must mount the ext4 loopback at `/sysroot/composefs` before `bootc-root-setup`
runs. Our `sysroot-composefs.mount` (in `xfs-mount.cpio`) has:
```ini
Before=initrd-root-fs.target bootc-root-setup.service
```

If the system does not mount the loopback in time, `bootc-root-setup.service` cannot open
the composefs repository (`Repository::open_upgrade(sysroot, "composefs")`).
Then the system falls back to the OSTree XFS root (Bluefin).

### Lesson learned

Any dracut-based initrd rebuild for composefs must use `--add bootc`. The
bootc module is opt-in and dracut excludes it by default. Without it, the initrd lacks
`bootc-root-setup.service`, the system never mounts the composefs EROFS as root,
and the system boots the OSTree deployment.

---

## 4. E2E Test Pipeline Architecture

### Problem

The E2E test script (`run-e2e.sh`) must do these steps:
1. Create a bootable VM disk with Bluefin LTS (XFS).
2. Boot the VM, connect through SSH, and run the migration.
3. Reboot into composefs (Dakota).
4. Validate state preservation, rollback, and commit.

This test takes more than 30 minutes. Debugging failures needs fast iteration.

### Architecture: checkpoint-based iteration

The script saves checkpoints:

| Checkpoint | When | What it saves |
|------------|------|---------------|
| `disk.raw.pre-migration` | After disk creation | Fresh Bluefin install |
| `disk.raw.post-migration` | After host-side scan | Migrated disk (composefs) |

Targets for fast iteration:

```bash
just e2e           # Full run (BTRFS)
just e2e-lts       # Full run (XFS)
just e2e-scan      # Host-side .raw scan only
just e2e-reboot-test  # Boot from checkpoint, validate composefs
just e2e-status    # Show current state
just e2e-ssh       # Interactive SSH
just e2e-tail      # Stream serial console
```

### Two-sided verification

| Side | What | Where | When |
|------|------|-------|------|
| **In-VM** | `verify_migration()` | `src/migration/mod.rs` | Inside QEMU after Phase 5 |
| **Host-side** | `.raw` disk scan | `tests/run-e2e.sh` | After QEMU shutdown, before reboot |

The host scan can find bugs in the filesystem that the VM cannot see (such as an unflushed initrd on VFAT or misplaced BLS entries).

### Lesson learned

In-VM verification cannot see filesystem issues because the page cache of the kernel hides them.
Always mount the raw disk image to verify from outside the VM.

---

## 5. XFS Loopback Workaround

### Problem

XFS does not support `fs-verity`, which composefs needs. The composefs
object store must reside on a filesystem that supports verity.

### Solution: ext4 loopback

Create an ext4 loopback image of 14 GB at `/sysroot/composefs-loopback.ext4`,
format it with `fs-verity` support, and mount it at `/sysroot/composefs`:

```rust
let img = "/sysroot/composefs-loopback.ext4";
// truncate to 14 GB
// mkfs.ext4 -O verity
// mount $img /sysroot/composefs
```

During boot, `sysroot-composefs.mount` (from `xfs-mount.cpio`) mounts this
loopback in the initrd. Then `bootc-root-setup.service` can find composefs
objects.

### Disk layout

```
/sysroot/composefs-loopback.ext4  # 14 GB, ext4, verity
/sysroot/composefs/               # mount point (ext4 loopback)
  objects/  # composefs content-addressed objects
  images/   # EROFS images
```

### Disk space constraints

| Component | Size |
|-----------|------|
| XFS root | ~19.5 GB (20 GB disk - ESP - BIOS) |
| Loopback | 14 GB |
| Bluefin OSTree | ~6 GB |
| Composefs objects | ~6 GB |
| Free | ~3 GB (tight) |

### Lesson learned

A 20 GB disk is barely enough for XFS, the loopback, and two OS images. The `podman cp`
approach fails at this size. Direct stream from the registry is mandatory.

---

## 6. SSH Reliability: The Bluefin SSH Problem

### Problem

Bluefin disables `sshd.service` by default. The E2E container image build
handles this problem with these steps:
1. It writes a `50-e2e-ssh.preset` file that enables sshd.
2. It creates `e2e-sshd.socket` and `e2e-sshd@.service` for TCP port 22
   (Bluefin sshd only listens on Unix-local and vsock sockets).
3. It injects the SSH public key into `authorized_keys` on the disk.

SSH connections can still fail intermittently. Sometimes they connect in 13 seconds.
Sometimes they fail after 180 seconds of retries.

### Root cause: sshd-keygen race

The `e2e-sshd@.service` runs `/usr/sbin/sshd -i` with `StandardInput=socket`.
In this mode, systemd handles the socket connection, and sshd runs as a per-connection service.

The command `sshd -i` needs host keys at `/etc/ssh/ssh_host_*key*`.
The service `sshd-keygen@.service` generates these keys. On Bluefin, `sshd-keygen`
runs in parallel with other boot services. The `e2e-sshd.socket` can accept
a connection before `sshd-keygen` creates the host keys.

When systemd starts `sshd -i` without host keys, sshd fails immediately with
`sshd: no hostkeys available` (exit code 255). The `-` prefix in
`ExecStart=-/usr/sbin/sshd -i` prevents systemd from an error log.
The client shows "Permission denied" instead of "Host key not found", because
`sshd -i` exits before public key authentication starts.

### Fix 1: Early sshd-keygen

Enable `sshd-keygen` before the socket accepts connections:

```bash
mkdir -p /etc/systemd/system/sshd-keygen.target.wants
ln -sf /usr/lib/systemd/system/sshd-keygen@.service \
       /etc/systemd/system/sshd-keygen.target.wants/sshd-keygen@rsa.service
ln -sf /usr/lib/systemd/system/sshd-keygen@.service \
       /etc/systemd/system/sshd-keygen.target.wants/sshd-keygen@ed25519.service
```

### Fix 2: Serial console automatic login

Override `serial-getty@ttyS0` to log in as root automatically:

```ini
[Service]
ExecStart=
ExecStart=-/sbin/agetty -o "-p -f root" --autologin root --noclear %I 115200 linux
```

This ensures we can always interact with the VM even when SSH fails.

### Fix 3: Key mismatch from checkpoints

The `disk.raw.post-migration` checkpoint has `authorized_keys` from one run.
The next run generates `test_key` again through `ssh-keygen`. This creates a new key that
does not match the old `authorized_keys` on disk.

This key mismatch causes most SSH failures, not a Bluefin bug. The script shows
"VM accessible via SSH after 13s" in fresh runs but "Permission denied" in checkpoint runs.

### Known SSH failure modes

| Symptom | Likely cause | Verdict |
|---------|-------------|---------|
| Permission denied (publickey) | Stale checkpoint, key mismatch | ~80% of failures |
| Connection refused | QEMU not running or port not bound | ~10% |
| Connection timeout | Firewall blocks connection | ~5% |
| sshd: no hostkeys available | sshd-keygen race | ~5% |

### Lessons

1. Always run `sudo rm -f disk.raw*` before a fresh run, because checkpoints hold stale keys.
2. Always create new session keys (`ssh-keygen -t rsa -N "" -f test_key -q`).
3. Always provide a serial console fallback for headless debugging.
4. Always enable `sshd-keygen` early to prevent races between host keys.

---

## 7. Stale Mounts and Checkpoint Contamination

### Problem

After an aborted E2E run, the host keeps:
- Stale loop devices (`/dev/loop0...loopN` attached to `disk.raw`).
- Stale mount points (`/tmp/mnt-e2e-esp-scan`, `-root-scan`, `-boot`, `-ckpt`).
- Extra mounts on top of existing mounts that form a multi-layer mount stack.
- Mount stacks where `find` hangs indefinitely while the kernel traverses all layers.
- Old checkpoint files (`disk.raw.post-migration`) with stale SSH keys.

### The mount stack bug

Each run of the host-side scan executes:

```bash
sudo mount "$HOST_ROOT" /tmp/mnt-e2e-root-scan
```

If the previous run did not unmount (stopped by Ctrl-C, `set -e`, or a signal),
the mount point remains active. The next run mounts on top of the existing mount.
After four aborted runs, `/tmp/mnt-e2e-root-scan` contains four stacked mounts:

```
/dev/loop0p3 on /tmp/mnt-e2e-root-scan  # run 1 (aborted)
/dev/loop1p3 on /tmp/mnt-e2e-root-scan  # run 2 (nouuid)
/dev/loop2p3 on /tmp/mnt-e2e-root-scan  # run 3
/dev/loop3p3 on /tmp/mnt-e2e-root-scan  # run 4 (current)
```

The `find` command walks through all stacked layers. Each layer transition
needs a kernel lookup call. With dead layers, `find` hangs while it resolves dentries.

### The checkpoint contamination bug

```bash
# Run 1: fresh run, test_key=RSA_KEY_A
# ... migration succeeds ...
# Post-migration checkpoint saved disk.raw with authorized_keys=RSA_KEY_A

# Run 2: resume from checkpoint
rm -f test_key
ssh-keygen -t rsa -N "" -f test_key  # generates RSA_KEY_B
cp disk.raw.post-migration disk.raw   # restores disk with RSA_KEY_A
# SSH fails: RSA_KEY_B ≠ RSA_KEY_A
```

The checkpoint captures a specific SSH key. The next run generates a new
key but uses the old disk from the checkpoint. Authentication fails
with "Permission denied (publickey)".

### Fixes

1. **Clean up at the start of each run**:
```bash
sudo umount /tmp/mnt-e2e-esp-scan 2>/dev/null || true
sudo umount /tmp/mnt-e2e-root-scan 2>/dev/null || true
sudo losetup -d /dev/loop0 2>/dev/null || true
sudo rm -f disk.raw disk.raw.*
```

2. **Use `-o nouuid` for XFS mounts**: prevents duplicate UUID errors that
   cause mount failures and find hangs.

3. **Re-seed keys after a checkpoint restore**: write the new public key
   into `authorized_keys`.

4. **Delete all checkpoints before full runs**:
   `sudo rm -f disk.raw disk.raw.pre-migration disk.raw.post-migration`

### Timeline

This bug consumed about 15 E2E runs. The host-side scan appeared to hang
without an error message. A check of mounts with `mount | grep mnt` showed the stack.
We first assumed a find bug on XFS, not a mount stack problem.

### Lessons

1. Unmount all paths before each run. An `umount` command on a stacked mount point can
   unmount only the top layer, and lower layers remain mounted.
2. Use `losetup -j disk.raw` to find all loop devices for a disk.
3. Checkpoints with authentication data are fragile. Generate keys again
   when you restore from a checkpoint.
4. When `find` hangs without an error, inspect mounts for stacked layers.

---

## 8. VFAT Sync: Zero-Byte Initrd Bug

### Symptoms

- In-VM `verify_migration()`: initrd is valid (200,915,858 bytes).
- Host-side `.raw` scan: initrd is 0 bytes.
- The ESP directory listing shows the file with the correct size.
  A read of the content returns nothing because the system never flushed data clusters to disk.

### Root cause

VFAT (FAT32) on Linux uses a writeback cache. The kernel writes file data to the
page cache immediately, but does not flush disk blocks until:
1. The process closes the file and the kernel evicts the inode.
2. The process calls `sync()`.
3. The system unmounts the filesystem.

Boot artifact extraction writes vmlinuz and initrd to the ESP
through layer streams from the registry (`extract_files_via_registry`). The function opens the
destination file with `File::create()`, writes data, and closes it. The VFAT
driver updates the directory entry, but data blocks remain dirty in the page cache.

`verify_migration()` then reads the initrd from the same mount, so it reads
valid data from the page cache. The verification succeeds.

When the VM shuts down and the host mounts the ESP cleanly, the page cache is cold.
The kernel reads from disk and returns zeros because the system never flushed data clusters.

### Why vmlinuz was valid but initrd was zero

The vmlinuz file (19.6 MB) occupies few FAT clusters. The initrd
(200 MB) spans hundreds of clusters. FAT32 can write small files to disk quickly,
but large cluster chains can remain in the cache.

### Fix

```rust
unsafe { libc::sync(); }
```

This flushes all dirty buffers to disk. Place it after boot artifact writes,
before `patch_origin_boot_digest()` reads the files to compute hashes.

Commit: `3245322`

### Timeline

- BTRFS tests passed because the migration writes directly to `/boot/` on
  the BTRFS root filesystem without VFAT. The migrator wrote to the ESP only
  during Phase 5 for systemd-boot.
- XFS tests showed the bug. The migration used the systemd-boot path
  to write to the VFAT ESP. Then the host scan read from the unmounted disk.
- We spent about 30 E2E runs on debug steps before a check of the raw disk with `xxd`.

### Lesson

Always call `sync()` after a write to VFAT or FAT32 before cross-mount
verification. The kernel page cache can hide missing disk flushes.

---

## 9. `set -euo pipefail` Pitfalls

This single feature of bash caused more E2E failures than any migration bug.
`set -euo pipefail` is standard in shell scripts, but in a long
integration test with SSH pipelines and background processes, it creates subtle bugs.

### Issue 1: SSH pipeline and dup2 stdout redirect

The original migration invocation was:
```bash
ssh ... "/var/tmp/bootc-migrate ..." 2>&1 \
  | awk '{ print "[migrate] " $0; fflush() }'
```

The migration binary calls `dup2(log_fd, STDOUT_FILENO)` to redirect stdout to
the log file. This closes stdout on the SSH channel. The local `awk` process sees EOF
and exits cleanly. With `set -o pipefail`, the pipeline returns the exit status of
the last command (`awk=0`). The background block completes, writes `MIGRATE_RC` to
`/tmp/e2e-migrate.rc`, and the parent script continues.

When the binary redirects stdout, the early awk pipe close causes a race condition.
If the parent script checks `wait` before the child writes `MIGRATE_RC`, the script receives
an empty variable, which triggers an error:

```bash
if [ "${MIGRATE_RC:-1}" != "0" ]; then exit 1; fi
```

Fix (`e3f5a42`): run the migration detached inside the VM through a heredoc.
Tail the log file to stream output, write the return code to a file, and fetch it after SSH exits.

Later fix (`f861bc9`): use a tee approach where a Rust background thread reads
from a pipe and writes to both stdout and the log file simultaneously.

### Issue 2: find, head -1, timeout, and pipefail

```bash
ORIGIN=$(find "$HOST_ROOT_MNT/state/deploy" -name '*.origin' 2>/dev/null | head -1)
```

When `find` encounters a slow filesystem (such as XFS with duplicate UUIDs),
it can hang. The `head -1` command closes the pipe, `find` receives SIGPIPE, and
`set -o pipefail` causes the command substitution to return non-zero.

On some bash versions, `set -e` does trigger on failures from command substitution,
even on variable assignment. The script exits without error output.

Fixes applied:
1. `-maxdepth 5` — prevents find from a full scan of the XFS tree.
2. `timeout 10` — stops find after 10 seconds.
3. `|| true` — prevents pipefail from a script exit.
4. `c3f420b` — clean stale mounts before each run to prevent hangs.

### Issue 3: CHECKPOINT unset variable

```bash
CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"
if [ -f "$CHECKPOINT" ]; then ...
elif [ -f "$POST_CKPT" ]; then ...
else
    SKIP_SETUP=false
    # CHECKPOINT was not set here -> set -u stops script on next use
fi
...
cp disk.raw "$CHECKPOINT"  # fails on unset variable
```

`set -u` treats any unset variable reference as a fatal error. When the script
creates a fresh disk (no checkpoint), it did not define `CHECKPOINT`.
The copy command stopped the script silently.

Result: the script created the disk and injected fixtures, but failed to save the checkpoint.
On the next run, the script had to start from scratch.

Fix: set `CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"` in the else branch so it is always defined.

### Issue 4: sudo mount failure

```bash
sudo mount "$HOST_ROOT" "$HOST_ROOT_MNT"
VMLINUZ=$(find "$HOST_ESP_MNT/EFI/Linux" -name vmlinuz 2>/dev/null | head -1)
```

If the mount fails, `set -e` does not trigger because no `||` guard follows mount.
The script continues with an empty mount point. The `find` command returns immediately,
and later `stat` commands fail on missing files.

Fix: add `|| exit 1` to mounts and use `-o nouuid` for XFS.

### Issue 5: In-VM diagnostic SSH failure

After migration completes, the script runs diagnostics inside the VM through SSH:

```bash
ssh $SSH_OPTS root@localhost bash <<'DIAG'
...
DIAG
```

If SSH fails during a VM reboot or network drop, the script exits.
With `set -e`, the failure stops the script and the host-side scan never runs.

Fix: add `|| true` to the diagnostic SSH command.

### Summary of pipefail fixes

| Issue | Symptom | Commit |
|-------|---------|--------|
| SSH pipe and dup2 close stdout | Script exits during migration | `e3f5a42`, `f861bc9` |
| find, head, timeout, pipefail | Script exits without error message | `1f963f8`, `66a0037`, `c3f420b` |
| CHECKPOINT unset | Script stops on fresh disk | `a0484dd` |
| sudo mount failure | Script exits silently | `743026e` |
| In-VM diagnostic SSH fails | Host-side scan never runs | `106547c`, `41bade2` |
| Awk backslash escape | Syntax error stops awk pipe | `4b3163f` |

### Lesson

`set -euo pipefail` helps with simple errors like missing files.
But it fails on subtle cases such as SIGPIPE in pipelines, unset variables,
or dropped SSH connections. Every command, pipeline, and variable expansion in a long
script must use an explicit `|| true`, a default value, or an error check.
A silent exit makes debug work much harder than explicit error checks.

---

## 10. OVMF NVRAM Persistence

### Problem

OVMF NVRAM (where firmware keeps entries for BootOrder) does not persist across QEMU
restarts unless:
1. QEMU uses `-machine q35` instead of `pc`.
2. You provide a writable pflash file for VARS.
3. The VARS file comes from a matched CODE and VARS build pair.
4. The VARS file has padding to match the CODE size.

### GRUB fallback

Because NVRAM persistence is fragile, the migration also configures the GRUB
`saved_entry` to the composefs BLS entry. This ensures composefs boots even
when OVMF resets BootOrder to shim and GRUB.

### Lesson learned

Do not rely only on UEFI NVRAM persistence in QEMU. Always configure a GRUB
fallback path.

---

## 11. Composefs Boot Blocker: The Missing Dracut Module

### Symptoms

- Migration completes all six phases (0 to 5).
- Host-side `.raw` scan shows valid vmlinuz (19.6 MB), initrd
  (220 MB), `systemd-boot.efi`, `.origin` file, and BLS entries.
- Direct `-kernel` QEMU boot with `composefs=<digest>` on the command line:
- EROFS mounts during initrd: `erofs: (device erofs): mounted...`.
- But the system displays `Welcome to Bluefin LTS` instead of Dakota.

### Investigation timeline

**Day 1:** GRUB boot configuration. Tested options:
- Set `default=0` in `grub.cfg` — ignored.
- Direct `menuentry 'Dakota (composefs)'` — GRUB ignored the custom entry.
- Modified ESP `grub.cfg` to bypass chainload — writes failed silently.

**Day 2:** QEMU direct boot bypasses GRUB.
- Boot with `-kernel`, `-initrd`, and `-append "composefs=..."` boots Bluefin.
- The system mounts the EROFS image (`erofs: mounted with root inode @ nid 36`).
- But the system does not use EROFS as root and boots the Bluefin OSTree.

Hypothesis: execution order between `ostree-prepare-root` and `bootc-root-setup`.
The system mounts EROFS too late (after switch-root), or cannot access the composefs
repository at `/sysroot/composefs/objects/` because the ext4 loopback is not active yet.

**Day 3:** Source code research through GitHub search.
- Found the bootc dracut module at `crates/initramfs/dracut/module-setup.sh`.
- `bootc-root-setup.service` has `ConditionKernelCommandLine=composefs`.
- The service runs `initramfs-setup setup-root`, which opens the composefs
  repository at `/sysroot/composefs/` and mounts the EROFS as root.

**Day 4:** Check whether the initrd includes the bootc module:
```bash
zcat initrd | cpio -t | grep bootc
# (no output — initrd lacks bootc module)
cat /usr/lib/dracut/modules.d/51bootc/module-setup.sh
check() {
    return 255  # never included automatically
}
```

### Root cause

The bootc dracut module uses `check() { return 255; }`, which means "do not
include this module unless explicitly requested." The initrd rebuild used:
```bash
dracut --force --kmoddir <kmoddir>
```

This creates a new initrd with default dracut modules from the host. Because
`51bootc/module-setup.sh` returns 255 from `check()`, dracut omits it.
The initrd lacks `bootc-root-setup.service`, `initramfs-setup`, and composefs root configuration.

Without `bootc-root-setup.service`, `ostree-prepare-root.service` still mounts the EROFS image,
but the system does not use it as the root filesystem. The initrd falls back to the XFS OSTree
deployment (Bluefin).

### The EROFS mount message was misleading

```
erofs: (device erofs): mounted with root inode @ nid 36.
```

The kernel outputs this message whenever it mounts any EROFS filesystem.
`ostree-prepare-root.service` mounts the composefs EROFS image at a side
path for verification. The system never mounted EROFS as root.
We spent a day to investigate why Bluefin booted despite the mount message.

### Fix

Include the dracut module for bootc explicitly:

```rust
cmd.arg("--add").arg("bootc");
```

This adds:
1. `/usr/lib/dracut/modules.d/51bootc/module-setup.sh`
2. `/usr/lib/systemd/system/bootc-root-setup.service`
3. `/usr/lib/bootc/initramfs-setup` (1.3 MB Rust binary)
4. `/usr/lib/composefs/setup-root-conf.toml` (if present)
5. Enables `bootc-root-setup.service` in `initrd-root-fs.target.wants`

Commit: `7291259`

### Lessons

1. An `erofs: mounted` log entry does not mean composefs is the root.
   The kernel could mount it at a temporary side path.
2. When `check() { return 255; }` is in a dracut module, dracut never
   includes it automatically. You must pass `--add <module>`.
3. Inspection of the bootc source code with `gh api` helped find the problem.
4. Always inspect what the initrd contains (`zcat initrd | cpio -t | grep`).
   The absence of `bootc-root-setup.service` revealed the root cause.
