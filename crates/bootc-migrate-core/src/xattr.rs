use anyhow::{Context, Result};
use rustix::fs::XattrFlags;
use rustix::io::Errno;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs as unix_fs;
use std::path::Path;

/// Initial buffer size for xattr name lists and values; grown on `ERANGE`.
const XATTR_BUF_INIT: usize = 256;

/// Copy a file preserving all extended attributes (SELinux, capabilities, user.*).
/// Tries a FICLONE reflink first — on btrfs/xfs a multi-GB tree copy becomes
/// metadata-only — and falls back to a plain copy when the clone fails
/// (cross-device, tmpfs, filesystem without reflink). Same bytes either way;
/// xattrs and permissions are applied after, on both paths.
pub fn copy_file_with_xattrs(src: &Path, dst: &Path) -> Result<()> {
    copy_file_with_xattrs_using(src, dst, try_reflink_paths)
}

/// Non-generic shim over the generic [`crate::reflink::reflink`] so it fits
/// the `fn(&Path, &Path)` slot in [`copy_file_with_xattrs_using`].
fn try_reflink_paths(src: &Path, dst: &Path) -> Result<()> {
    crate::reflink::reflink(src, dst)
}

/// The plain read/write fallback when FICLONE is unavailable.
fn plain_file_copy(src: &Path, dst: &Path) -> Result<()> {
    let mut src_file = fs::File::open(src)
        .with_context(|| format!("failed to open src for xattr copy: {}", src.display()))?;
    let mut dst_file = fs::File::create(dst)
        .with_context(|| format!("failed to create dst for xattr copy: {}", dst.display()))?;

    let mut buffer = [0u8; 65536];
    loop {
        let n = src_file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        dst_file.write_all(&buffer[..n])?;
    }
    Ok(())
}

/// [`copy_file_with_xattrs`] with the clone step injectable, so tests pin
/// the try-reflink-then-fallback order deterministically on any filesystem.
fn copy_file_with_xattrs_using(
    src: &Path,
    dst: &Path,
    try_reflink: fn(&Path, &Path) -> Result<()>,
) -> Result<()> {
    if try_reflink(src, dst).is_err() {
        plain_file_copy(src, dst)?;
    }

    // Copy extended attributes from src to dst
    copy_xattrs(src, dst)?;

    // Copy permissions
    let metadata = src.metadata()?;
    let mode = unix_fs::PermissionsExt::mode(&metadata.permissions());
    let mut perms = fs::metadata(dst)?.permissions();
    unix_fs::PermissionsExt::set_mode(&mut perms, mode);
    fs::set_permissions(dst, perms)?;

    Ok(())
}

/// Copy all extended attributes from `src` to `dst` (follows symlinks).
///
/// Shared by [`copy_file_with_xattrs`] and the `/etc` merge in
/// [`crate::mergetc`]. Best-effort: a destination filesystem without xattr
/// support (`ENOTSUP`, e.g. a FAT32 ESP) is silently tolerated; other set
/// failures are logged but do not abort the copy.
pub fn copy_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let Some(names) = list_xattr_names(src)? else {
        return Ok(());
    };

    // The kernel returns names as a NUL-separated list.
    for name_bytes in names.split(|b| *b == 0) {
        if name_bytes.is_empty() {
            continue;
        }
        let name = CString::new(name_bytes)?;
        let Some(value) = get_xattr_value(src, &name)? else {
            continue;
        };

        match rustix::fs::setxattr(dst, name.as_c_str(), &value, XattrFlags::empty()) {
            Ok(()) => {}
            // ENOTSUP is expected on filesystems without xattr support (FAT32 ESP).
            Err(Errno::NOTSUP) => {}
            Err(e) => eprintln!(
                "Warning: failed to set xattr '{}' on {}: {}",
                String::from_utf8_lossy(name_bytes),
                dst.display(),
                e
            ),
        }
    }

    Ok(())
}

/// Return the NUL-separated list of xattr names on `path`, or `None` when the
/// file has no xattrs or the filesystem doesn't support them.
fn list_xattr_names(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; XATTR_BUF_INIT];
    loop {
        match rustix::fs::listxattr(path, &mut buf[..]) {
            Ok(0) => return Ok(None),
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(Errno::RANGE) => buf.resize(buf.len() * 2, 0),
            Err(Errno::NOTSUP) | Err(Errno::NODATA) => return Ok(None),
            Err(e) => return Err(e).context("listxattr failed"),
        }
    }
}

/// Read the value of a single xattr, or `None` if it vanished or is unreadable.
fn get_xattr_value(path: &Path, name: &CString) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; XATTR_BUF_INIT];
    loop {
        match rustix::fs::getxattr(path, name.as_c_str(), &mut buf[..]) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(Errno::RANGE) => buf.resize(buf.len() * 2, 0),
            Err(Errno::NODATA) | Err(Errno::NOTSUP) => return Ok(None),
            Err(e) => return Err(e).context("getxattr failed"),
        }
    }
}

/// Copy a directory tree recursively, preserving extended attributes on all files.
pub fn copy_dir_all_with_xattrs(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let mut ctx = CopyCtx {
        progress: None,
        counts: CopyCounts::default(),
    };
    copy_dir_all_inner(src.as_ref(), dst.as_ref(), &mut ctx)
}

/// Running totals reported by [`copy_dir_all_with_xattrs_progress`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CopyCounts {
    /// Regular files and symlinks copied so far (directories excluded).
    pub files: u64,
    /// Bytes of regular-file content copied so far.
    pub bytes: u64,
    /// Special files skipped so far (sockets, devices, overlay whiteouts).
    pub specials_skipped: u64,
}

/// What the Nth skipped special file (1-based) prints.
#[derive(Debug, PartialEq, Eq)]
enum SpecialNotice {
    /// Full `skipping … at <path>` line.
    Full,
    /// Running tally line.
    Tally,
    /// Nothing — the tally covers it.
    Silent,
}

/// Throttle for skip warnings: full lines for the first 10, then a tally
/// every 1000. A live /var holds hundreds of thousands of overlay whiteouts;
/// one line each buries the log it is trying to inform.
fn special_notice(n: u64) -> SpecialNotice {
    if n <= 10 {
        SpecialNotice::Full
    } else if n.is_multiple_of(1000) {
        SpecialNotice::Tally
    } else {
        SpecialNotice::Silent
    }
}

/// Copy a directory tree like [`copy_dir_all_with_xattrs`], calling
/// `progress` with cumulative [`CopyCounts`] after each file or symlink so
/// multi-GB copies (a live `/var`) can show movement instead of silence.
/// The callback decides its own throttle; counting itself is just integers.
pub fn copy_dir_all_with_xattrs_progress(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    progress: &mut dyn FnMut(CopyCounts),
) -> Result<CopyCounts> {
    let mut ctx = CopyCtx {
        progress: Some(progress),
        counts: CopyCounts::default(),
    };
    copy_dir_all_inner(src.as_ref(), dst.as_ref(), &mut ctx)?;
    Ok(ctx.counts)
}

/// Shared recursion state: the optional progress callback plus its
/// cumulative counts, so neither needs reborrowing gymnastics per level.
struct CopyCtx<'a> {
    progress: Option<&'a mut dyn FnMut(CopyCounts)>,
    counts: CopyCounts,
}

impl CopyCtx<'_> {
    fn copied_file(&mut self, bytes: u64) {
        if let Some(cb) = self.progress.as_deref_mut() {
            self.counts.files += 1;
            self.counts.bytes += bytes;
            cb(self.counts);
        }
    }
}

fn copy_dir_all_inner(src: &Path, dst: &Path, ctx: &mut CopyCtx) -> Result<()> {
    fs::create_dir_all(dst)?;
    // Preserve directory mode (umask would otherwise mask it to 755, which
    // breaks sshd StrictModes on dirs like /root/.ssh that must be 700).
    let src_meta = fs::metadata(src)?;
    let src_mode = unix_fs::PermissionsExt::mode(&src_meta.permissions());
    let mut dst_perms = fs::metadata(dst)?.permissions();
    unix_fs::PermissionsExt::set_mode(&mut dst_perms, src_mode);
    fs::set_permissions(dst, dst_perms)?;
    let _ = copy_xattrs(src, dst);
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(file_name);
        let ty = entry.file_type()?;

        if ty.is_dir() {
            copy_dir_all_inner(&path, &dest_path, ctx)?;
        } else if ty.is_symlink() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            let link_target = fs::read_link(&path)?;
            std::os::unix::fs::symlink(link_target, &dest_path)?;
            ctx.copied_file(0);
        } else if ty.is_file() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            copy_file_with_xattrs(&path, &dest_path)?;
            // The extra stat runs only when somebody is counting.
            let bytes = if ctx.progress.is_some() {
                fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };
            ctx.copied_file(bytes);
        } else {
            ctx.counts.specials_skipped += 1;
            match special_notice(ctx.counts.specials_skipped) {
                SpecialNotice::Full => {
                    eprintln!("Warning: skipping special file at {:?}", path);
                }
                SpecialNotice::Tally => {
                    eprintln!(
                        "Warning: …{} special files skipped so far \
                         (sockets, devices, overlay whiteouts)",
                        ctx.counts.specials_skipped
                    );
                }
                SpecialNotice::Silent => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // TDD tests for xattr-preserving copy.

    #[test]
    fn copy_file_with_xattrs_preserves_data() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");

        fs::write(&src, b"hello xattr test").unwrap();
        copy_file_with_xattrs(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello xattr test");
    }

    #[test]
    fn copy_file_with_xattrs_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.sh");
        let dst = dir.path().join("dst.sh");

        fs::write(&src, b"#!/bin/sh\necho hi").unwrap();
        let mut perms = fs::metadata(&src).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&src, perms).unwrap();

        copy_file_with_xattrs(&src, &dst).unwrap();

        let dst_perms = fs::metadata(&dst).unwrap().permissions();
        assert_eq!(dst_perms.mode() & 0o777, 0o755);
    }

    #[test]
    fn copy_file_with_xattrs_handles_no_xattrs() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("plain.txt");
        let dst = dir.path().join("copied.txt");

        fs::write(&src, b"no xattrs here").unwrap();
        // Should succeed even without any xattrs.
        copy_file_with_xattrs(&src, &dst).unwrap();
        assert!(dst.exists());
    }

    #[test]
    fn copy_dir_all_with_xattrs_preserves_symlinks() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink("real.txt", src_dir.join("link.txt")).unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();

        assert!(dst_dir.join("real.txt").exists());
        let link_target = fs::read_link(dst_dir.join("link.txt")).unwrap();
        assert_eq!(link_target.to_string_lossy(), "real.txt");
    }

    #[test]
    fn copy_dir_all_with_xattrs_preserves_directory_mode() {
        // Regression: sshd StrictModes rejects authorized_keys when its parent
        // .ssh dir is anything looser than 700. The recursive copy must
        // propagate the source dir mode rather than inheriting umask 022.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        let ssh = src_dir.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(ssh.join("authorized_keys"), b"ssh-rsa AAA").unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();

        let dst_ssh_mode = fs::metadata(dst_dir.join(".ssh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            dst_ssh_mode & 0o777,
            0o700,
            "dst .ssh must stay 700, got {:o}",
            dst_ssh_mode & 0o777
        );
    }

    #[test]
    fn copy_dir_all_with_xattrs_skips_special_files() {
        // Just verify it doesn't panic on an empty dir with no special files
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("f.txt"), b"data").unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();
        assert!(dst_dir.join("f.txt").exists());
    }

    #[test]
    fn copy_file_tries_reflink_first() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CLONE_ATTEMPTED: AtomicBool = AtomicBool::new(false);
        fn stub_ok(src: &Path, dst: &Path) -> Result<()> {
            CLONE_ATTEMPTED.store(true, Ordering::SeqCst);
            plain_file_copy(src, dst)
        }

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, b"clone me").unwrap();

        CLONE_ATTEMPTED.store(false, Ordering::SeqCst);
        copy_file_with_xattrs_using(&src, &dst, stub_ok).unwrap();

        assert!(CLONE_ATTEMPTED.load(Ordering::SeqCst));
        assert_eq!(fs::read(&dst).unwrap(), b"clone me");
    }

    #[test]
    fn copy_file_falls_back_when_reflink_fails() {
        fn stub_err(_src: &Path, _dst: &Path) -> Result<()> {
            anyhow::bail!("FICLONE unavailable here")
        }

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, b"plain copy").unwrap();

        copy_file_with_xattrs_using(&src, &dst, stub_err).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"plain copy");
    }

    #[test]
    fn special_notice_throttles_after_ten_then_tallies_per_thousand() {
        use SpecialNotice::{Full, Silent, Tally};
        for n in [1, 2, 10] {
            assert_eq!(special_notice(n), Full, "n={n}");
        }
        for n in [11, 12, 999, 1001, 1999] {
            assert_eq!(special_notice(n), Silent, "n={n}");
        }
        for n in [1000, 2000, 1_000_000] {
            assert_eq!(special_notice(n), Tally, "n={n}");
        }
    }

    #[test]
    fn copy_dir_all_with_xattrs_progress_counts_files_and_bytes() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        fs::create_dir_all(src_dir.join("sub")).unwrap();
        fs::write(src_dir.join("a.txt"), b"12345").unwrap();
        fs::write(src_dir.join("sub").join("b.txt"), b"1234567890").unwrap();
        std::os::unix::fs::symlink("a.txt", src_dir.join("link")).unwrap();

        let mut seen = Vec::new();
        let total =
            copy_dir_all_with_xattrs_progress(&src_dir, &dst_dir, &mut |c| seen.push(c)).unwrap();

        // Two files (5 + 10 bytes) plus one symlink; directories excluded.
        assert_eq!(
            total,
            CopyCounts {
                files: 3,
                bytes: 15,
                specials_skipped: 0
            }
        );
        assert_eq!(seen.last().copied(), Some(total));
        assert!(
            seen.windows(2)
                .all(|w| w[1].files >= w[0].files && w[1].bytes >= w[0].bytes),
            "progress must be cumulative, got {seen:?}"
        );
        assert!(dst_dir.join("a.txt").exists());
        assert!(dst_dir.join("link").is_symlink());
    }
}
