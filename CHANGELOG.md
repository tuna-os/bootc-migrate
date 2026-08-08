# Changelog

All notable changes to `bootc-migrate` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions correspond to GitHub Releases; the binary embeds the git SHA at
build time (`bootc-migrate --version`).

---

## [Unreleased]

### Added
- `bootc-rebase de-migrate stash|restore` — move a user's desktop-environment
  config into/out of a stash directory around a cross-DE re-base, plus a
  best-effort portable-preference extractor and a `pre-switch.d`/
  `post-switch.d` hook contract (#68).
- `bootc-rebase boot-entries` — read-only UEFI boot-entry audit: classifies
  entries as dead, generic-label, duplicate, or firmware-managed (#31).
- `bootc-migrate etc-drift` — computes the factory-vs-live `/etc`
  diff (Added/Modified/Removed/TypeChanged) as a table or JSON, ahead of a
  migration (#15).
- `bootc-migrate etc-drift --interactive` and `--review-drift` (Phase 0.5) —
  an interactive ratatui checklist over the `/etc` config drift; unchecked
  entries take the target's new default instead of the live version. The
  resulting decisions manifest wires into Phase 4's 3-way `/etc` merge via
  `merge_etc_files_with_overrides`, preserving any overridden user content
  as a `.rebase-old` sidecar (#15).
- `bootc-rebase scan` capability probe extended with transient-root/etc,
  fs-verity-required, initramfs composefs-module presence, filesystem
  expectation, and a `Compatible: YES/NO` verdict with reasons (#24).
- Cross-base UID/GID remap (`bootc-rebase --accept-cross-base`) now applies
  to the staged `OstreeDeploy` deployment, not just the report (#67 part 1).
- E2E: dbus/logind health assertions after the `ostree-rebase` cell, guarding
  against identity-DB regressions in `bootc switch`'s native `/etc` merge.
- `bootc-rebase migrate-bootloader` subcommand shape and its pure BLS-entry/
  kernel-arg/entry-token core (#65) — the subcommand always refuses; see
  ROADMAP.md for what's deliberately not implemented yet.

### Fixed
- Remove debug kernel arguments (`systemd.log_level=debug`,
  `systemd.log_target=console`, `systemd.journald.forward_to_console=1`) that
  were accidentally left in production `kernel_options.rs`. These caused every
  migrated system to boot with verbose journal output on the console.

### Changed
- Transferred repository to `tuna-os/bootc-migrate`.
- Expanded `CONTRIBUTING.md` with full E2E environment setup, debugging
  guide, scenario table, and dependency update policy.
- Updated `AGENTS.md` CI matrix to reflect the actual four E2E scenarios
  (was missing the LVM-on-LUKS + dedicated `/var` scenario).
- `README.md`: added `undo` subcommand to the troubleshooting table, added
  `/var` independence warning to the `commit` step, fixed E2E scenario count.

---

## [v0.4.0] — 2026-06

### Added
- **LVM-on-LUKS E2E scenario** — full coverage for Bluefin LTS systems with a
  dedicated `/var` logical volume (xfs+lvm+crypt, 40 GB disk). The kernel
  cmdline builder now discovers and emits `rd.lvm.lv=<vg>/<lv>` for every
  mounted LV, ensuring the composefs target image activates non-root LVs
  during initrd. Validated end-to-end on every CI push.
- `just e2e-lvm` recipe and matching CI matrix entry.
- `watcher.sh` — log-tail script for monitoring long-running E2E tests; exits
  on error patterns or idle timeout. Available as `just watch`.

### Fixed
- XFS systems without native fs-verity now correctly create an ext4 loopback
  device at `/sysroot/composefs-loopback.ext4` for verity support.
- Bootc version compatibility: `composefs.rs` falls back to `podman run
  --privileged` with the target image's own bootc when the host bootc is ≤1.13
  (missing `oci-manifest-*` stream support).
- Free-space heuristic for XFS/loopback paths raised to 1.5× (was 1.1× like
  btrfs, which was too tight).

---

## [v0.3.0] — 2026-05

### Added
- **LUKS + XFS E2E scenario** (xfs+crypt, 40 GB disk with swtpm TPM2
  emulation). LUKS `rd.luks.name` / `rd.luks.uuid` / `rd.luks.options` args
  are now carried through from the source cmdline to the composefs BLS entry.
- `undo` and `undo --full` subcommands for post-migration cleanup without
  committing.
- `--bootloader grub2` flag: stay on GRUB2 instead of installing systemd-boot
  (for BIOS or firmware-quirky systems).
- `--force` flag: proceed past non-fatal preflight warnings.
- `SPECIFICATION.md` — detailed on-disk layout reference (OSTree + ComposeFS
  backends, migration plan, test rig design).
- `docs/filesystem-support.md` — btrfs vs XFS divergence documented with
  summary table.
- `docs/architecture.md` — architecture decisions and lessons learned.
- `docs/luks-testing.md` — LUKS E2E design notes.

### Changed
- Phase 5 bootloader extraction switched from EROFS bare mount (which
  zero-fills file content past ~4 KB) to **registry streaming** — downloads
  OCI layers iteratively (fetch → extract needed files → delete blob → repeat),
  bounding peak disk usage to ~200 MB per layer.
- `commit` subcommand reclaims ~14 GiB by removing the OSTree object store.

---

## [v0.2.0] — 2026-04

### Added
- **XFS + ext4 loopback E2E scenario** (Bluefin LTS path).
- `--skip-import` flag: skip Phase 1 OSTree reflink import (faster for mostly
  new content).
- `--dry-run` flag: print every planned action without touching disk.
- `--skip-preflight` flag: bypass preflight checks.
- `commit` subcommand: one-way finalization that removes the OSTree fallback
  and reclaims disk.
- Phase 4: identity-DB line-union for `/etc/passwd`, `/etc/shadow`,
  `/etc/group`, `/etc/gshadow`, `/etc/subuid`, `/etc/subgid`.
- Phase 4: dangling `/usr/*` symlink pruning.
- `mergetc.rs` — 3-way `/etc` merge including file→symlink type-change
  handling across image lineages.
- `xattr.rs` — file copy with SELinux, capabilities, and `user.*` xattr
  preservation.
- Release workflow: x86_64 + aarch64 prebuilt binaries, SHA-256 checksums.

### Fixed
- `bootc status` now correctly reports `composefs` backend after migration
  (`manifest_digest` written in `.origin` via tini).
- SSH key permissions preserved during `/var` copy.

---

## [v0.1.0] — 2026-03

### Added
- Initial implementation of the OSTree → ComposeFS in-place migration for
  Bluefin stable → Dakota (btrfs, x86_64).
- Six-phase architecture: Preflight → OSTree import → OCI pull → EROFS seal →
  Stage deploy → Bootloader.
- QEMU-based E2E harness (`tests/run-e2e.sh`).
- Default CI: clippy + rustfmt + unit tests + shellcheck (`just check`).
- E2E CI: btrfs scenario on every push to `main`.
- `justfile` with build, test, E2E, lint, and cleanup recipes.

[Unreleased]: https://github.com/tuna-os/bootc-migrate/compare/HEAD...main
[v0.4.0]: https://github.com/tuna-os/bootc-migrate/releases/tag/v0.4.0
[v0.3.0]: https://github.com/tuna-os/bootc-migrate/releases/tag/v0.3.0
[v0.2.0]: https://github.com/tuna-os/bootc-migrate/releases/tag/v0.2.0
[v0.1.0]: https://github.com/tuna-os/bootc-migrate/releases/tag/v0.1.0
