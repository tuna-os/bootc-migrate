//! systemd-boot-specific bootloader operations.
//!
//! Home for the standalone `migrate-bootloader` subcommand's (issue #65)
//! pure, backend-agnostic logic: kernel-argument carry-over, entry-token
//! derivation, and BLS entry assembly. Live ESP/NVRAM mutation and the
//! kernel-install resync hook are implemented in the sibling `live` module,
//! which the CLI routes through `rebase_controller`.
//!
//! The composefs migrator's own systemd-boot setup is unrelated and
//! untouched: it lives in `migration::phase5_setup_bootloader`.

use super::BlsEntry;

/// Carry a kernel command line over to a new BLS entry, dropping any
/// argument that starts with one of `strip_prefixes` (e.g. `"composefs="`,
/// which is composefs-specific and must not appear on a plain-OSTree
/// migrate-bootloader entry). Order and spacing of the surviving arguments
/// is normalized to single spaces.
pub fn carry_over_kargs(cmdline: &str, strip_prefixes: &[&str]) -> String {
    cmdline
        .split_whitespace()
        .filter(|arg| !strip_prefixes.iter().any(|p| arg.starts_with(p)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The systemd `entry-token` used to namespace `$BOOT/<token>/<version>/…`
/// paths (see `kernel-install(8)`): the trimmed contents of
/// `/etc/kernel/entry-token` if present and non-empty, else the machine id
/// verbatim. No file I/O here — callers hand in whatever they already read,
/// so this stays pure and unit-testable.
pub fn derive_entry_token(entry_token_file: Option<&str>, machine_id: &str) -> String {
    match entry_token_file.map(str::trim) {
        Some(token) if !token.is_empty() => token.to_string(),
        _ => machine_id.trim().to_string(),
    }
}

/// The ESP-relative paths systemd-boot expects for a kernel/initrd pair
/// under a given entry token + version, per the `$BOOT/<token>/<version>/`
/// layout `kernel-install(8)` and `bootctl(1)` use.
pub fn esp_kernel_paths(entry_token: &str, version: &str) -> (String, String) {
    let base = format!("/{entry_token}/{version}");
    (format!("{base}/linux"), format!("{base}/initrd"))
}

/// Assemble the primary BLS entry for a `migrate-bootloader` re-base: the
/// current OSTree deployment's kernel, on the ESP, with its live kargs
/// (`composefs=`-stripped) carried forward. `title`/`filename`/`sort_key`
/// are the caller's choice; ESP/ostree path plumbing, NVRAM registration,
/// and the kernel-install resync hook are out of scope here — see the
/// module-level docs.
pub fn build_migrate_bootloader_entry(
    title: &str,
    version: &str,
    entry_token: &str,
    cmdline: &str,
    filename: &str,
    sort_key: &str,
) -> BlsEntry {
    let (linux, initrd) = esp_kernel_paths(entry_token, version);
    let options = carry_over_kargs(cmdline, &["composefs="]);
    BlsEntry {
        title: title.to_string(),
        version: version.to_string(),
        linux,
        initrds: vec![initrd],
        options,
        filename: filename.to_string(),
        sort_key: sort_key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carry_over_kargs_drops_composefs_param() {
        let cmdline = "root=UUID=abc rw quiet composefs=deadbeef splash";
        let carried = carry_over_kargs(cmdline, &["composefs="]);
        assert_eq!(carried, "root=UUID=abc rw quiet splash");
    }

    #[test]
    fn carry_over_kargs_drops_multiple_prefixes() {
        let cmdline = "root=UUID=abc composefs=deadbeef ostree=/ostree/boot.1/x rw";
        let carried = carry_over_kargs(cmdline, &["composefs=", "ostree="]);
        assert_eq!(carried, "root=UUID=abc rw");
    }

    #[test]
    fn carry_over_kargs_no_matches_is_unchanged_modulo_spacing() {
        let cmdline = "  root=UUID=abc   rw  quiet ";
        let carried = carry_over_kargs(cmdline, &["composefs="]);
        assert_eq!(carried, "root=UUID=abc rw quiet");
    }

    #[test]
    fn carry_over_kargs_empty_cmdline() {
        assert_eq!(carry_over_kargs("", &["composefs="]), "");
    }

    #[test]
    fn entry_token_prefers_override_file() {
        let token = derive_entry_token(Some("my-token\n"), "1234567890abcdef");
        assert_eq!(token, "my-token");
    }

    #[test]
    fn entry_token_falls_back_to_machine_id_when_file_absent() {
        let token = derive_entry_token(None, "1234567890abcdef\n");
        assert_eq!(token, "1234567890abcdef");
    }

    #[test]
    fn entry_token_falls_back_to_machine_id_when_file_empty() {
        let token = derive_entry_token(Some("   \n"), "1234567890abcdef");
        assert_eq!(token, "1234567890abcdef");
    }

    #[test]
    fn esp_kernel_paths_use_token_and_version() {
        let (linux, initrd) = esp_kernel_paths("mytoken", "6.8.0-1");
        assert_eq!(linux, "/mytoken/6.8.0-1/linux");
        assert_eq!(initrd, "/mytoken/6.8.0-1/initrd");
    }

    #[test]
    fn build_entry_strips_composefs_and_sets_esp_paths() {
        let entry = build_migrate_bootloader_entry(
            "Bluefin",
            "6.8.0-1",
            "mytoken",
            "root=UUID=abc composefs=deadbeef rw",
            "bluefin-0.conf",
            "bootc-rebase-0",
        );
        assert_eq!(entry.title, "Bluefin");
        assert_eq!(entry.version, "6.8.0-1");
        assert_eq!(entry.linux, "/mytoken/6.8.0-1/linux");
        assert_eq!(entry.initrds, vec!["/mytoken/6.8.0-1/initrd".to_string()]);
        assert_eq!(entry.options, "root=UUID=abc rw");
        assert_eq!(entry.filename, "bluefin-0.conf");
        assert_eq!(entry.sort_key, "bootc-rebase-0");
        // Rendered form must not leak the composefs param onto a
        // plain-OSTree entry (the documented bootc-parser trap).
        assert!(!entry.render().contains("composefs="));
    }
}
