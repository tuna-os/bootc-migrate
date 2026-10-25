//! The kernel argument that names the composefs image a host boots.
//!
//! bootc writes it in one of two forms, and a host can carry either:
//!
//! - `composefs=<hex>`: the original form. A leading `?` marks an image booted
//!   without signature enforcement, and some writers put `sha512:` before the
//!   hex. Neither is part of the digest.
//! - `composefs.digest=v1-sha512-12:<hex>`: the form newer bootc writes. The
//!   part before the last `:` names the format, the hash and the block size.
//!
//! Every reader of the argument goes through this module, so a host booted
//! with either form is detected as composefs and gives the same digest.
//! `rd.composefs=` and `ostree.composefs=` are different arguments and do not
//! match.

/// The argument prefixes, for callers that drop the argument from a command
/// line they carry over.
pub const PREFIXES: &[&str] = &["composefs=", "composefs.digest="];

/// Whether one kernel argument is a composefs image argument.
pub fn is_arg(arg: &str) -> bool {
    PREFIXES.iter().any(|prefix| arg.starts_with(prefix))
}

/// Whether the kernel command line names a composefs image.
pub fn present(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(is_arg)
}

/// The hex digest in one composefs image argument, or `None` when `arg` is
/// not one or its value is empty.
pub fn arg_digest(arg: &str) -> Option<&str> {
    let digest = if let Some(value) = arg.strip_prefix("composefs=") {
        let value = value.trim_start_matches('?');
        value.strip_prefix("sha512:").unwrap_or(value)
    } else {
        let value = arg.strip_prefix("composefs.digest=")?;
        value.rsplit_once(':').map_or(value, |(_, hex)| hex)
    };
    (!digest.is_empty()).then_some(digest)
}

/// The hex digest of the composefs image the kernel command line names.
pub fn digest(cmdline: &str) -> Option<&str> {
    cmdline.split_whitespace().find_map(arg_digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_forms_name_the_same_digest() {
        for (cmdline, expected) in [
            ("rw composefs=0f1e quiet", Some("0f1e")),
            ("composefs=?0f1e", Some("0f1e")),
            ("composefs=sha512:0f1e", Some("0f1e")),
            (
                "rw composefs.digest=v1-sha512-12:0f1e lockdown=integrity",
                Some("0f1e"),
            ),
            ("composefs.digest=0f1e", Some("0f1e")),
            ("composefs=", None),
            ("composefs.digest=v1-sha512-12:", None),
            ("rd.composefs=0f1e ostree.composefs=1", None),
            ("ostree=/ostree/boot.1/default/abc/0 rw", None),
            ("", None),
        ] {
            assert_eq!(digest(cmdline), expected, "{cmdline}");
        }
    }

    #[test]
    fn present_matches_either_form_only() {
        for (cmdline, expected) in [
            ("rw composefs=0f1e quiet", true),
            ("rw composefs.digest=v1-sha512-12:0f1e", true),
            ("rd.composefs=0f1e ostree.composefs=1", false),
            ("ostree=/ostree/boot.1/default/abc/0 rw", false),
        ] {
            assert_eq!(present(cmdline), expected, "{cmdline}");
        }
    }

    #[test]
    fn only_composefs_image_arguments_match() {
        for arg in ["composefs=abc", "composefs.digest=v1-sha512-12:abc"] {
            assert!(is_arg(arg), "{arg}");
        }
        for arg in [
            "rd.composefs=abc",
            "ostree.composefs=0",
            "composefs",
            "composefs.other=1",
            "root=UUID=x",
        ] {
            assert!(!is_arg(arg), "{arg}");
        }
    }
}
