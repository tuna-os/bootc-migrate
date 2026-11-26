# CONTEXT.md — domain glossary and architecture map

Orientation for contributors and agents. Deep dives live in
`docs/architecture.md` (decisions + lessons learned) and the RFCs
(#30 re-base engine, #45 library carve-out — both closed/resolved but
still the best design narrative).

## Workspace map

```
crates/
  bootc-migrate-core/        # the library — everything reusable
  bootc-migrate/   # the proven migrator binary + TUI  ← protected MVP
  bootc-rebase/              # universal re-base engine (growing per RFC #30)
```

**Invariant: `bootc-migrate` is the working MVP.** Its CLI surface,
printed output, and behavior do not change; the four composefs E2E matrix
cells are untouchable regression gates. New capability lands additively in
`bootc-rebase` and `bootc-migrate-core`. Shared-core refactors must keep the
migrator's path byte-compatible.

## Domain glossary

- **bootc** — boot-and-update system for bootable OCI containers; the host OS
  is an OCI image. Has two root-storage **backends**: OSTree and ComposeFS.
- **OSTree backend** — classic: deployments are hardlink checkouts of ostree
  commits under `/sysroot/ostree/deploy/...`; BLS entries live in
  `/boot/loader/entries` and OSTree rewrites them on every kernel update.
- **ComposeFS backend** — the deployment is a sealed EROFS image whose files
  are fs-verity-protected objects in `/sysroot/composefs/objects`; the initrd
  (`bootc-root-setup.service`, dracut module `51bootc`, opt-in via
  `--add bootc`) mounts it as root using the `composefs=<verity>` karg.
- **verity digest vs sealed config digest** — two distinct identifiers that
  are easy to confuse: the *fs-verity digest* names the EROFS rootfs image
  (used in `composefs=` and `.origin`); the *sealed config digest*
  (`sha256:…`) names the OCI config stream that `bootc internals cfs oci
  mount` needs (`streams/oci-config-<digest>`). Passing the wrong one gets a
  zero-filling raw-EROFS fallback mount (see docs/architecture.md §1).
- **EROFS zero-fill trap** — a bare EROFS mount returns zeros for file content
  past ~4 KB/inode. Any large file (vmlinuz, initrd, kernel modules) must come
  from the overlay mount or **registry streaming**.
- **registry streaming** (`core::registry`) — download OCI layers one at a
  time: fetch → extract wanted paths → delete blob. Peak disk ≈ one layer.
  The workhorse for boot-artifact extraction on tight disks.
- **BLS entry** — Boot Loader Specification type-1 config
  (`loader/entries/*.conf`). ⚠️ bootc's composefs parser treats every non-EFI
  BLS entry *on the ESP* as a composefs deployment and errors without a
  `composefs=` param — never put plain OSTree entries on the ESP.
- **ESP** — EFI System Partition (vfat). systemd-boot reads its entries from
  here; XBOOTLDR partitions must be vfat too (sd-boot ≥258.2), so ext4
  `/boot` cannot be handed to sd-boot by GUID-retyping (issue #65).
- **stateroot / deployment / staged** — OSTree vocabulary: a *stateroot*
  holds `/var`; a *deployment* is one bootable root; *staged* means prepared
  for the next boot. A **pending transaction** (staged/pending deployment or
  stale repo tmp files) blocks migration — see `preflight`.
- **3-way /etc merge** (`core::mergetc`) — old-default ∆ current → new-default;
  preserves per-machine edits while adopting target defaults. OSTree does
  this natively on deploy; the composefs migration does it via `mergetc`.
- **reflink** — CoW file clone (FICLONE). With it, object import is
  near-free; without it, migration needs ~1.5× repo size.
- **XFS loopback** — XFS lacks fs-verity, so the store lives in an ext4
  loopback image at `/sysroot/composefs-loopback.ext4` mounted over
  `/sysroot/composefs`.
- **two-phase transaction** (`core::transaction`) — migrations stage next to
  the existing deployment, both bootable; `commit` makes it permanent
  (one-way, deletes the OSTree side), `undo` rolls back (preserves the store
  unless `--full`).
- **cfs CLI generations** (issue #72) — upstream bootc removed `oci
  create-image`/`oci seal` (creation folded into `pull --bootable` /
  `prepare-boot`, sealing implicit). *Legacy* = ships `create-image`; probed
  via `bootc internals cfs oci --help`. Store selection (`core::composefs`):
  host bootc if legacy → target image's bootc if legacy → pinned legacy
  builder (`BMC_CFS_BUILDER`, default fedora-bootc). Probe asymmetry is
  deliberate: a failed *host* probe counts as legacy (fail-safe fast path); a
  failed *container* probe counts as NOT legacy (an ENOSPC'd podman run must
  not masquerade as a legacy verdict).
- **store format vs writing CLI** — new-generation bootc *reads*
  legacy-format stores fine (proven by green LTS→dakota E2E cells); only the
  writing CLI changed. Hence any legacy-CLI bootc is a valid store writer.
  The permanent fix is the **native backend** (`core::composefs_native`,
  feature `composefs-native`, issue #13): write the store with the
  composefs/composefs-oci crates directly, selected when the *target* is
  new-generation (the store format is defined by the bootc that reads it at
  boot).
- **route / strategy** (`core::rebase_plan`) — the transition table:
  (source backend → target backend) → strategy (`CoreMigration`,
  `OstreeDeploy`, `ImageSwap`, `OstreeInstall`, `PackageInstall`) +
  implemented flag. First match wins; the CLI refuses unimplemented routes.

## Core library tour (`bootc-migrate-core`)

- `migration/` — the phase pipeline; each phase independently callable:
  `import` (P1: OSTree objects → store), `pull` (P2: OCI pull), `seal`
  (P3: create+seal EROFS), `deploy` (P4: /etc merge, /var, state root),
  `boot` (P5: kernel/initrd extraction, BLS, bootloader, OSTree fallback).
  `run_migration()` in `migration/mod.rs` is the canonical composition.
- `composefs` — `ComposefsStore` trait: `BootcCliStore` (drives
  `bootc internals cfs`, host or target-image bootc via podman) +
  `MockComposefsStore` for tests/dry-runs.
- `preflight` — `SystemInfo::gather()` (generic introspection),
  `validate::*` (per-direction verdicts), `readiness` (report printing +
  `gate()` → `Proceed | ConfirmFullCopy | Refuse`).
- `registry`, `mergetc`, `xattr`, `reflink`, `ostree`, `transaction`,
  `types::VerityDigest`.

## E2E harness (`tests/run-e2e.sh` + `e2e-tests.yml`)

- Matrix **cells** = (base image, target image, `FILESYSTEM`, `DISK_SIZE`,
  `E2E_MODE`). Modes: `composefs-migrate` (default, the four MVP cells),
  `ostree-rebase` (scenario A cell), `ostree-rebase-plan` (route tracer).
- LUKS cells answer the passphrase prompt by **polling** the serial log with
  grep (the prompt has no trailing newline — a line-buffered reader misses
  it; PR #58).
- Guest ENOSPC during Phase-2 pulls is the classic flake — fixed by disk
  sizing (40G+), not retries. GHCR "connection reset" early in a pull is a
  genuine network flake; rerun.
- `gh run rerun` reuses the run's original merge commit — it does **not**
  pick up fixes merged to main afterward; merge main into the PR branch
  instead.

## Conventions

- Every new behavior merges behind a new or extended E2E cell.
- Squash merges; PR titles conventional-commit style.
- `just` recipes for fmt/clippy/test (`just fmt-check` is what CI runs).
- Lints are workspace-level and strict (`unsafe_code = "deny"`,
  `missing_debug_implementations = "deny"` — public lib types need `Debug`).
