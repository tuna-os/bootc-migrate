# Architecture Decisions & Lessons Learned

## Overview

This document captures key architectural decisions, workarounds, and lessons
learned during development of the OSTree→ComposeFS live migration tool. It
covers both the migrator binary (`src/`) and the E2E test harness
(`tests/run-e2e.sh`).

---

## 1. Boot Artifact Extraction

### Problem
Phase 5 needs to copy kernel, initrd, and systemd-boot binaries from the
target container image (Dakota) to the ESP or `/boot`. The composefs EROFS
overlay mount returns zero-filled file content past an inline data threshold
(~4 KB per inode), corrupting large files like vmlinuz (17 MB) and initrd.

### Attempted solutions (in order)

| Approach | Peak disk | Time | Result |
|----------|-----------|------|--------|
| bootc cfs overlay mount | 0 | instant | Returns zeros for large files |
| podman cp | 6 GB (full image) | 30s | Works but ENOSPC with 14 GB loopback |
| skopeo copy | 6 GB+ | 60s | Same ENOSPC problem |
| Registry streaming | 500 MB | ~10 min | ✅ **Final** — downloads layers one at a time |

### Architecture decision: Registry streaming

`extract_files_via_registry()` and `extract_subtree_via_registry()` download
OCI layers iteratively: fetch → extract needed files → delete blob → repeat.
This bounds peak disk usage to the largest single layer (~200 MB).

Used for: vmlinuz, initrd, systemd-bootx64.efi, kernel modules (`/usr/lib/modules/<kver>/`).

### Lesson learned
EROFS bare-mount zero-fills content past 4 KB inline threshold. Any tool
reading large files from a composefs EROFS mount must use the `bootc internals
cfs oci mount` overlay mount or stream from the registry.

---

## 2. Kernel Module Extraction for Initrd Rebuild

### Problem
XFS root requires `xfs.ko` in the initrd. The EROFS mount can't provide
kernel modules (zero-filled). `podman cp` pulls the full target image
(~6 GB) into podman storage, hitting ENOSPC on the 20 GB disk with 14 GB
loopback allocated.

### Attempted solutions

| Approach | Peak disk | Disk needed | Free on XFS | Result |
|----------|-----------|-------------|-------------|--------|
| EROFS mount path | 0 | 0 | 3 GB | Zero-filled |
| podman cp | 6 GB | 6 GB | 3 GB | ENOSPC ❌ |
| containers-storage: | 0 | 0 | 3 GB | Image not in podman |
| Registry streaming | 500 MB | 1 GB | 3 GB | ✅ |

### Architecture decision: Registry streaming for kmods

`extract_subtree_via_registry(target_image, "usr/lib/modules/<kver>", tempdir)`
streams the kernel module subtree layer-by-layer. Peak: largest single layer
(~200 MB) + extracted tree (~300 MB) + new initrd (~80 MB) ≈ 580 MB.

Free-space check lowered from 6 GB to 1.5 GB.

### Lesson learned
`podman cp` requires pulling the full image into podman storage first.
`containers-storage:` transport requires the image to already be pulled.
Neither works when disk is tight. Registry streaming is the only viable path.

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

`return 255` means the module is **never automatically included** — it requires
explicit `dracut --add bootc`. Our initrd rebuild was using plain
`dracut --force --kmoddir` without `--add bootc`.

### Fix
Add `--add bootc` to the dracut command for all initrd rebuilds:
```rust
let dracut_add = format!("{} bootc", mods_str);
cmd.arg("--add").arg(&dracut_add);
```

### Where bootc dracut module lives
- Module: `/usr/lib/dracut/modules.d/51bootc/module-setup.sh`
- Service: `/usr/lib/systemd/system/bootc-root-setup.service`
- Binary: `/usr/lib/bootc/initramfs-setup` (1.3 MB Rust binary)
- Config: `/usr/lib/composefs/setup-root-conf.toml` (optional)

The dracut module installs these files into the initramfs via its `install()`
function. The bootc module is opt-in because `check() { return 255; }` means
dracut never includes it automatically. You must pass `--add bootc`.

### EROFS mount ordering in initrd

`bootc-root-setup.service` has:
```
ConditionKernelCommandLine=composefs
After=sysroot.mount
After=ostree-prepare-root.service
Before=initrd-root-fs.target
```

The ext4 loopback at `/sysroot/composefs` must be up before `bootc-root-setup`
runs. Our `sysroot-composefs.mount` (in xfs-mount.cpio) has:
```
Before=initrd-root-fs.target bootc-root-setup.service
```

If the loopback isn't mounted in time, `bootc-root-setup.service` can't open
the composefs repository (`Repository::open_upgrade(sysroot, "composefs")`),
and the system falls back to the OSTree xfs root (Bluefin).

### Lesson learned
Any dracut-based initrd rebuild for composefs must use `--add bootc`. The
bootc module is opt-in and excluded by default. Without it, the initrd lacks
`bootc-root-setup.service`, the composefs EROFS is never mounted as root,
and the system boots the OSTree deployment.

---

## 4. E2E Test Pipeline Architecture

### Problem
The E2E test script (`run-e2e.sh`) must:
1. Create a bootable VM disk with Bluefin LTS (XFS)
2. Boot it, SSH in, run the migration
3. Reboot into composefs (Dakota)
4. Validate state preservation, rollback, commit

This is a 30+ minute test. Debugging failures requires fast iteration.

### Architecture: checkpoint-based iteration

The script saves checkpoints:
| Checkpoint | When | What it saves |
|------------|------|---------------|
| `disk.raw.pre-migration` | After disk creation | Fresh Bluefin install |
| `disk.raw.post-migration` | After host-side scan | Migrated disk (composefs) |

Just targets for fast iteration:

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

The host-side scan catches filesystem-level bugs the VM can't see (0-byte
initrd from VFAT not flushing, BLS entries on wrong partition).

### Lesson learned
In-VM verification can't see filesystem-level issues because the kernel's
page cache hides them. Always verify from outside the VM by mounting the
raw disk image.

---

## 5. XFS Loopback Workaround

### Problem
XFS doesn't support `fs-verity`, which composefs requires. The composefs
object store must be on a verity-capable filesystem.

### Solution: ext4 loopback
Create a 14 GB ext4 loopback image at `/sysroot/composefs-loopback.ext4`,
format it with `fs-verity` support, and mount it at `/sysroot/composefs`:

```rust
let img = "/sysroot/composefs-loopback.ext4";
// truncate to 14 GB
// mkfs.ext4 -O verity
// mount $img /sysroot/composefs
```

During boot, `sysroot-composefs.mount` (from xfs-mount.cpio) mounts this
loopback in the initrd so `bootc-root-setup.service` can find composefs
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
| Free | ~3 GB (tight!) |

### Lesson learned
20 GB is barely enough for XFS + loopback + two OS images. The podman cp
approach fails at this size. Registry streaming is mandatory.

---

## 6. SSH Reliability: The Bluefin ssDH Problem

### Problem
Bluefin disables `sshd.service` by default. The E2E container image build
works around this by:
1. Writing a `50-e2e-ssh.preset` that enables sshd
2. Creating `e2e-sshd.socket` + `e2e-sshd@.service` for TCP port 22
   (Bluefin's sshd only listens on Unix-local and vsock sockets)
3. Injecting the SSH public key into the disk's authorized_keys

But SSH connections are UNRELIABLE. Sometimes they work in 13 seconds.
Sometimes they never work, even after 180 seconds of retries.

### Root Cause: sshd-keygen race

The `e2e-sshd@.service` runs `/usr/sbin/sshd -i` with `StandardInput=socket`.
This is the "unencrypted" sshd mode where the socket connection is handled
by systemd, and sshd runs as a per-connection service.

sshd -i requires host keys to be present at `/etc/ssh/ssh_host_*key*`.
These are generated by `sshd-keygen@.service`. On Bluefin, sshd-keygen
runs in parallel with other boot services. The e2e-sshd.socket can accept
a connection BEFORE sshd-keygen finishes generating host keys.

When `sshd -i` is started without host keys, it fails immediately with
`sshd: no hostkeys available` (exit code 255). The `-` prefix in
`ExecStart=-/usr/sbin/sshd -i` means systemd doesn't record the failure.
The client sees "Permission denied" — not "Host key not found" — because
sshd -i exits before even offering public key authentication.

### Fix 1: Early sshd-keygen

Enable every host-key generator as a member of `sshd-keygen.target`. Keep
`e2e-sshd.socket` free to start with `sockets.target`; make each
`e2e-sshd@.service` wait for that target before it starts `sshd -i`. This
makes the first connection wait for key creation. It also avoids a unit-order
cycle during boot:
```bash
mkdir -p /etc/systemd/system/sshd-keygen.target.wants
for key in rsa ecdsa ed25519; do
    ln -sf /usr/lib/systemd/system/sshd-keygen@.service \
        "/etc/systemd/system/sshd-keygen.target.wants/sshd-keygen@${key}.service"
done
```
Put `Wants=sshd-keygen.target` and `After=sshd-keygen.target` on the
per-connection service, not the socket. Socket units start before
`sockets.target`. If a socket waits for the keygen target, systemd can create a
cycle through `basic.target`.

### Fix 2: Serial console auto-login

Override serial-getty@ttyS0 to auto-login as root:
```
[Service]
ExecStart=
ExecStart=-/sbin/agetty -o "-p -f root" --autologin root --noclear %I 115200 linux
```

This ensures we can always interact with the VM even when SSH fails.

### Fix 3: test_key mismatch from checkpoints

The `disk.raw.post-migration` checkpoint has authorized_keys from one run.
Next run regenerates `test_key` via `ssh-keygen`, creating a NEW key that
doesn't match the OLD authorized_keys on disk.

This is the MOST COMMON cause of SSH failure — not a Bluefin bug but a
stale checkpoint. The script shows "VM accessible via SSH after 13s" in
fresh runs but "Permission denied" in checkpoint-restored runs.

### Known SSH failure modes
| Symptom | Likely cause | Verdict |
|---------|-------------|---------|
| Permission denied (publickey) | Stale checkpoint, key mismatch | ~80% of failures |
| Connection refused | QEMU not running / port not bound | ~10% |
| Connection timeout | Firewalld blocking, host unreachable | ~5% |
| sshd: no hostkeys available | sshd-keygen race | ~5% |

### Lessons
1. Always `sudo rm -f disk.raw*` before a fresh run — checkpoints carry stale keys
2. Always regenerate session keys fresh (`ssh-keygen -t rsa -N "" -f test_key -q`)
3. Always provide a serial console fallback for headless debugging
4. Always enable sshd-keygen early to avoid host key races

---

## 7. Stale Mounts and Checkpoint Contamination

### Problem
After an aborted E2E run, the host is left with:
- Stale loop devices (`/dev/loop0...loopN` attached to disk.raw)
- Stale mount points (`/tmp/mnt-e2e-esp-scan`, `-root-scan`, `-boot`, `-ckpt`)
- Each new run mounts ANOTHER loop device on TOP of the existing mount,
  creating a multi-layered mount stack
- `find` on these stacked mounts hangs indefinitely (kernel traverses
  through all layers)
- Old checkpoint files (`disk.raw.post-migration`) have stale SSH keys

### The mount stacking bug
Each run of the host-side scan does:
```bash
sudo mount "$HOST_ROOT" /tmp/mnt-e2e-root-scan
```

If the previous run didn't unmount (killed by Ctrl-C, `set -e` exit, or
signal), the mount point is still active. The NEXT run's `sudo mount`
succeeds — it mounts on TOP of the existing mount. After 3-4 aborted runs,
the path `/tmp/mnt-e2e-root-scan` has 4 stacked mounts:
```
/dev/loop0p3 on /tmp/mnt-e2e-root-scan  # run 1 (aborted)
/dev/loop1p3 on /tmp/mnt-e2e-root-scan  # run 2 (nouuid)
/dev/loop2p3 on /tmp/mnt-e2e-root-scan  # run 3
/dev/loop3p3 on /tmp/mnt-e2e-root-scan  # run 4 (current)
```

`find` on this path walks through ALL stacked layers. Each layer transitions
require kernel `__d_lookup` calls. With multiple dead layers, find hangs
indefinitely trying to resolve dentries across the mount stack.

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

The checkpoint captures a specific SSH key. The next run regenerates the
key but uses the checkpoint's old disk. Authentication fails silently
with "Permission denied (publickey)".

### Fixes

1. **Cleanup at start of each run**:
```bash
sudo umount /tmp/mnt-e2e-esp-scan 2>/dev/null || true
sudo umount /tmp/mnt-e2e-root-scan 2>/dev/null || true
sudo losetup -d /dev/loop0 2>/dev/null || true
sudo rm -f disk.raw disk.raw.*
```

2. **-o nouuid for XFS mounts**: prevents duplicate UUID errors that
   cause mount failure and cascading find hangs

3. **Regenerate key after restoring checkpoint**: re-seed authorized_keys
   from the fresh public key

4. **Delete all checkpoints before full runs**:
   `sudo rm -f disk.raw disk.raw.pre-migration disk.raw.post-migration`

### Timeline
This bug wasted ~15 E2E runs. Symptoms appeared as "host-side scan hangs"
with no error message. Only noticing the stacked mounts via `mount | grep mnt`
revealed the cause. The initial assumption was a find bug on XFS or a slow
filesystem — not a mount stacking problem.

### Lessons
1. Kill ALL mounts before each run — `umount` on a stale mount point might
   unmount the TOP layer only, leaving the stack underneath.
2. Use `losetup -j disk.raw` to find ALL loop devices for a given disk.
3. Checkpoints with authentication material are fragile. Regenerate keys
   when restoring from checkpoint.
4. `find` hanging without error is almost always a mount issue, not a
   filesystem issue.

---

## 8. VFAT Sync: Zero-Byte Initrd Bug

### Symptoms
- In-VM `verify_migration()`: ✅ initrd is valid (200,915,858 bytes)
- Host-side `.raw` scan: ❌ initrd is 0 bytes
- The ESP directory listing shows the file with correct size, but reading
  the content returns nothing — the directory entry is correct (shows 200 MB)
  but the file data clusters were never flushed to disk.

### Root Cause
VFAT (FAT32) on Linux uses writeback caching: file data is written to the
kernel's page cache immediately, but the actual disk blocks aren't scheduled
for writeback until:
1. The file is closed AND the inode is evicted from cache
2. `sync()` is called explicitly
3. The filesystem is unmounted (which triggers a full sync)

The migration's boot artifact extraction writes vmlinuz + initrd to the ESP
via registry streaming (`extract_files_via_registry`). The function opens the
destination file with `File::create()`, writes data, and closes it. The VFAT
driver updates the directory entry (file size, timestamps) but the data
blocks are still dirty in the page cache.

`verify_migration()` then reads the initrd from the SAME mount — and gets
real data from the page cache. The verification passes.

But when the VM shuts down and the host-side `.raw` scan mounts the ESP
fresh, the page cache is cold. The kernel reads from disk and gets zeros
because the file data clusters were never flushed.

### Why vmlinuz was fine but initrd was zero
vmlinuz (19.6 MB) fits in a small number of FAT clusters. The initrd
(200 MB) spans hundreds of clusters. With FAT32's loose cluster chain
flushing, small files often get written back opportunistically while large
files' cluster chains may remain unflushed indefinitely.

### Fix
```rust
unsafe { libc::sync(); }
```
This flushes ALL dirty buffers to disk. Placed after boot artifact writes,
before `patch_origin_boot_digest()` reads them back for hashing.

Commit: `3245322`

### Timeline
- BTRFS tests passed because the migration writes directly to `/boot/` on
  the btrfs root filesystem (no VFAT involved). The ESP was only written
  during Phase 5 for systemd-boot.
- XFS tests showed the bug because the migration used the systemd-boot path
  (writes to VFAT ESP), then the host-side scan read from the unmounted disk.
- ~30 E2E runs wasted debugging "why in-VM passes but host-side fails"
  before someone looked at the raw disk with `xxd` and saw zeros.

### Lesson
Always `sync()` after writing to VFAT or FAT32 before any cross-mount
verification. The kernel's page cache lies to you — especially for large
files.

---

## 9. `set -euo pipefail` Gotchas: The Silent Script Killer

This single bash feature caused more E2E failures than any migration bug.
`set -euo pipefail` is standard in modern shell scripts, but in a 1200-line
integration test with SSH pipelines, background processes, and disk mounts,
it's a landmine field.

### Issue 1: SSH pipeline + dup2 stdout redirect

The original migration invocation was:
```bash
ssh ... "/var/tmp/bootc-migrate ..." 2>&1 \
  | awk '{ print "[migrate] " $0; fflush() }'
```

The migration binary calls `dup2(log_fd, STDOUT_FILENO)` to redirect stdout/to
the log file. This closes the SSH channel's stdout (the dup2 replaces fd 1
with the log file). The local `awk` process sees EOF, exits cleanly — but
with `set -o pipefail`, the pipeline's exit status is the LAST command's
exit status (awk=0). The background block completes, writes MIGRATE_RC to
/tmp/e2e-migrate.rc, and the parent script continues.

BUT: when the migration binary's stdout is redirected, the awk pipe closing
early causes a race. If MIGRATE_RC isn't written before the parent checks
`wait`, the script gets an empty MIGRATE_RC, which triggers:
```bash
if [ "${MIGRATE_RC:-1}" != "0" ]; then exit 1; fi
```

The fix (`e3f5a42`): run migration detached inside the VM via a heredoc,
tail the log file for streaming, write rc to a marker file, fetch it after
ssh exits.

Later fix (`f861bc9`): the tee approach — a Rust background thread reads
from a pipe and writes to BOTH the original stdout and the log file
simultaneously.

### Issue 2: find + head -1 + timeout + pipefail

```bash
ORIGIN=$(find "$HOST_ROOT_MNT/state/deploy" -name '*.origin' 2>/dev/null | head -1)
```

When `find` encounters a slow filesystem (e.g. XFS with duplicate UUID
causing mount to fall back to single-user mode), it hangs. The `head -1`
closes the pipe, find gets SIGPIPE, and with `set -o pipefail`, the
command substitution returns non-zero.

On some bash versions, `set -e` DOES trigger on command substitution
failures even when assigned to a variable. The script exits without
error output.

Fixes applied:
1. `-maxdepth 5` — prevents find from scanning the entire XFS tree
2. `timeout 10` — kills find after 10 seconds
3. `|| true` — prevents pipefail from propagating
4. `c3f420b` — clean stale mounts before each run (prevents double-mount
   stacking that made find hang)

### Issue 3: CHECKPOINT unbound variable

```bash
CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"
if [ -f "$CHECKPOINT" ]; then ...
elif [ -f "$POST_CKPT" ]; then ...
else
    SKIP_SETUP=false
    # CHECKPOINT NEVER SET HERE → set -u KILLS SCRIPT ON NEXT USE
fi
...
cp disk.raw "$CHECKPOINT"  # ← BOOM: unbound variable
```

`set -u` makes any reference to an unset variable a fatal error. When
creating a fresh disk (no checkpoint), `CHECKPOINT` was never defined.
The `cp disk.raw "$CHECKPOINT"` at line 428 killed the script silently.

Result: the disk was created, fixtures injected, but the save failed.
On next re-run, no checkpoint existed, and the script started from scratch.

Fix: `set default value: CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"`
in the else branch, so it's always set.

### Issue 4: sudo mount failure

```bash
sudo mount "$HOST_ROOT" "$HOST_ROOT_MNT"
VMLINUZ=$(find "$HOST_ESP_MNT/EFI/Linux" -name vmlinuz 2>/dev/null | head -1)
```

If the mount fails (e.g. duplicate XFS UUID), `set -e` doesn't trigger
because there's no `||` after mount. The script continues with an empty
mount point. The `find` on an empty directory returns immediately (nothing
found), but the script then tries to `stat` a non-existent file, which
fails, and FINALLY `set -e` triggers.

Fix: add `|| exit 1` to mounts and use `-o nouuid` for XFS.

### Issue 5: In-VM diag SSH failure

After migration completes, the script runs in-VM diagnostics via SSH:
```bash
ssh $SSH_OPTS root@localhost bash <<'DIAG'
...
DIAG
```

If this SSH fails (VM rebooting, connection drops), the script exits.
With `set -e`, the failure propagates immediately — the host-side scan
never runs.

Fix: `|| true` on the diag SSH.

### Compilation of pipefail fixes
| Issue | Symptom | Commit |
|-------|---------|--------|
| SSH pipe + dup2 closes stdout | Script exits mid-migration | `e3f5a42`, `f861bc9` |
| find + head + timeout + pipefail | Script exits, no error msg | `1f963f8`, `66a0037`, `c3f420b` |
| CHECKPOINT unset | Script kills on fresh disk | `a0484dd` |
| sudo mount failure | Script exits silently | `743026e` |
| In-VM diag SSH fails | Host-side scan never runs | `106547c`, `41bade2` |
| Awk backslash escape | Syntax error kills awk pipe | `4b3163f` |

### Lesson
`set -euo pipefail` handles the simple cases (missing binaries, permission
denied) but fails catastrophically on subtle failures like find SIGPIPE,
unset variables, or SSH connection drops. Every command, pipeline, and
variable expansion in a 1200-line integration script must have an explicit
`|| true`, default value, or error guard. The debugging cost of a silent
`set -e` exit far outweighs the benefit of catching early failures.

---

## 10. OVMF NVRAM Persistence

### Problem
OVMF NVRAM (where BootOrder entries are stored) doesn't persist across QEMU
restarts unless:
1. QEMU uses `-machine q35` (not pc)
2. A writable VARS pflash file is provided
3. The VARS file is from a matched CODE+VARS pair (same build)
4. The VARS file is properly padded to match CODE size

### GRUB fallback
Because NVRAM persistence is fragile, the migration ALSO configures GRUB's
`saved_entry` to the composefs BLS entry. This ensures composefs boots even
when OVMF resets BootOrder to shim → GRUB.

### Lesson learned
Never rely on UEFI NVRAM persistence in QEMU. Always configure a GRUB
fallback path.

## 11. Composefs Boot Blocker: The Missing Dracut Module

### Symptoms
- Migration completes all 6 phases (0–5) ✅
- Host-side `.raw` scan shows valid vmlinuz (19.6 MB, MZ magic), initrd
  (220 MB), systemd-boot.efi, .origin file, BLS entries ✅
- Direct `-kernel` QEMU boot with `composefs=<digest>` on cmdline:
- EROFS mounts during initrd: `erofs: (device erofs): mounted...` ✅
- But the system shows `Welcome to Bluefin LTS` — NOT Dakota ❌

### Investigation timeline

**Day 1:** GRUB boot configuration. Attempted:
- `set default=0` in grub.cfg — ignored
- Direct `menuentry 'Dakota (composefs)'` — GRUB ignored custom menuentry
- Modified ESP grub.cfg to bypass chainloading — writes failed silently

**Day 2:** QEMU direct boot bypasses GRUB entirely.
- `-kernel` + `-initrd` + `-append "composefs=..."` boots Bluefin
- EROFS image IS mounted (`erofs: mounted with root inode @ nid 36`)
- But EROFS is NOT used as root — system boots Bluefin OSTree

Hypothesis: ostree-prepare-root vs bootc-root-setup ordering. The EROFS
mount might be happening too late (after switch-root), or the composefs
repository at `/sysroot/composefs/objects/` might not be accessible because
the ext4 loopback isn't mounted in time.

**Day 3:** Source code research via GitHub search.
- Found bootc dracut module at `crates/initramfs/dracut/module-setup.sh`
- `bootc-root-setup.service` has `ConditionKernelCommandLine=composefs`
- The service runs `initramfs-setup setup-root` which opens the composefs
  repository at `/sysroot/composefs/` and mounts the EROFS as root

**Day 4:** Checked if bootc module is in the initrd:
```
$ zcat initrd | cpio -t | grep bootc
# (no output — bootc module NOT in initrd)
$ cat /usr/lib/dracut/modules.d/51bootc/module-setup.sh
check() {
    return 255  # ← NEVER automatically included!
}
```

### Root cause

The bootc dracut module has `check() { return 255; }` which means "never
include me unless explicitly requested." The initrd rebuild was using:
```
dracut --force --kmoddir <kmoddir>
```

This creates a new initrd from scratch using the host system's dracut
module defaults. Since `51bootc/module-setup.sh` returns 255 from `check()`,
dracut doesn't include it — no `bootc-root-setup.service`, no
`initramfs-setup` binary, no composefs root setup in the initrd.

Without `bootc-root-setup.service`, the composefs EROFS IS mounted (by
`ostree-prepare-root.service` which has built-in composefs support) but is
NOT used as the root filesystem. The initrd falls back to the XFS OSTree
deployment (Bluefin).

### The EROFS mount was misleading

`erofs: (device erofs): mounted with root inode @ nid 36.`

This message comes from the kernel when ANY erofs filesystem is mounted.
`ostree-prepare-root.service` mounts the composefs EROFS image at a side
path (not as root) for verification. The EROFS mount was never the root.
Over a day of investigation was wasted on "EROFS mounts but system boots
Bluefin" because the mount message looks like success.

### Fix

Include the bootc dracut module explicitly:
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
1. `erofs: mounted` does NOT mean composefs is the root. It could be
   mounted anywhere — even a temporary side mount.
2. When `check() { return 255; }` in a dracut module, the module is NEVER
   auto-included. You must pass `--add <module>`.
3. Reading the bootc source code (GitHub search via `gh api`) was the
   key breakthrough — without it, we'd still be debugging GRUB config.
4. Always check what's ACTUALLY in the initrd (`zcat initrd | cpio -t | grep`).
   The absence of `bootc-root-setup.service` was the smoking gun.
