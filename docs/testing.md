# Testing and automation

This document explains how this project runs tests, gates pull requests, and
automates validation. This text complements `ROADMAP.md` and
`docs/cfs-cli-generations.md`.

## The pyramid

| Layer | What | Where it runs | Budget |
|---|---|---|---|
| Unit | Pure logic: parsers, planners, mergetc tables, routing, digest handling, probe classification | `just test` / CI `validate`, every push | seconds |
| Feature-matrix | Same, with every cargo feature on (`composefs-native` is invisible to the default build) | `just test-all-features` (part of `just check`) | seconds |
| Integration (host) | Real repositories on loopback filesystems, real podman probes of real images — no VM | developer machines + scheduled canary | minutes |
| E2E (VM) | Full migrations in QEMU: partition → install → migrate → reboot → assert | e2e-tests.yml matrix, PR-gated via `e2e-gate` | ~30 min/cell |

Rules of thumb:

- **Move logic down the pyramid.** Test behavior without a VM when possible.
  The E2E cells find issues that need a real boot: initrd behavior, bootloader
  handoff, and mount namespaces.
- **A failed E2E cell must name its phase.** The harness (`tests/run-e2e.sh`)
  prints `=== Phase N ===` banners and asserts with `FAIL:` prefixes so logs
  are searchable (`just e2e-failures`).
- **Use evidence instead of assumptions.** When upstream behavior is unclear,
  run the experiment before you write code. `docs/cfs-cli-generations.md` is
  the worked example.

## E2E matrix: current and planned

Current status of regression gates and additions:

| Cell | Proves | Status |
|---|---|---|
| bluefin stable → dakota (btrfs) | new-gen host path: probe → delegation ladder → builder | active |
| bluefin LTS → dakota (ext4) | legacy fast path, ext4 fs-verity | active |
| bluefin LTS → dakota (xfs+LUKS) | loopback store, passphrase injection | active |
| bluefin LTS → dakota (xfs+LVM+LUKS, split /var) | worst-case storage stack | active |
| bluefin stable → bluefin gts (ostree-rebase mode) | OstreeDeploy strategy + rollback presence | PR #69/#70 |
| bluefin stable → aurora (ostree-rebase mode) | cross-DE native `/etc` merge probe (#80) | active, non-gating |
| bluefin stable → dakota (tui-migrate mode) | TUI wizard + Config Drift Review event loops on a pty (`tests/tui-e2e-driver.py`), then the full composefs pipeline + all default-mode assertions | active, gating |
| dakota stable (composefs-native) → fedora-bootc 44 (`composefs-to-ostree` mode) | the reverse backend switch (#260): `--plan` resolves `OstreeInstall`, the target's `bootc install to-existing-root` runs alongside, `/etc` + `/var` + `/var/home` fixtures survive, the reboot lands in the OSTree deployment, and the composefs "Linux Boot Manager" entry and ESP kernel remain as rollback | active, non-gating |
| dakota stable (composefs-native) → utah testing (`image-swap` mode) | a composefs image swap across distributions: `--plan` resolves `ImageSwap`, the host's `bootc switch` stages Utah (Bluefin on Fedora Hummingbird), `/etc` + `/var` + `/var/home` fixtures survive, the reboot lands in Utah and the Dakota deployment stays as rollback | active, non-gating |
| bluefin stable → bootcrew/opensuse-bootc (`E2E_CROSS_FAMILY=1`) | the cross-family gate refuses without `--accept-cross-base`; with it, the cross-family `/etc` policy (#256): target defaults win, `.rebase-old` sidecars, carried machine state, target-first identity merge, first-boot unit | active, non-gating |

### Cross-base mode (`E2E_CROSS_BASE=1`)

The `ostree-rebase` mode takes `E2E_CROSS_BASE=1`. This adds `--accept-cross-base`.
The system refuses the route without this flag. The test asserts that
`=== Cross-base UID/GID remap report ===` appears in the output.

The assertion is necessary. `gate_cross_base` returns `None` and prints nothing
when it declines to act. Silence will look like success, so an unasserted cell
passes without proof.

Issue #187 tracks this work. The blocker is clear now, and differs from
earlier assumptions.

The diagnostic produced this output on 2026-08-28:

```
could not reach registry ghcr.io
  (https: token fetch failed: curl: (22) The requested URL returned error: 403
 ; http: unexpected status from http://ghcr.io/v2/: 301)
```

The guest can reach ghcr.io. The `http` try received a redirect to HTTPS.
The `https` try received a `401` challenge, parsed it, and requested a token.
That token request returned **403**. `curl` is present, DNS resolves, and TLS
works. The failure occurs in `fetch_bearer_token`.

Earlier text said the scan could not reach ghcr.io. That was incorrect.
`bootc switch` pulls successfully later because `containers/image` constructs
its token request differently. Our token request has a format error.

A 403 error points to an invalid request format or scope. Issue #187 tracks
the fix.

Two changes make this blocker easy to diagnose:

- `RegistryEndpoint::resolve` reports what each scheme returned, instead of a
  single error string.
- The scan retry uses four tries with exponential backoff (~14 seconds).
  A missing `curl` returns immediately.

The `ostree-rebase` path also checks registry access (`[registry-probe]` lines:
`curl` presence, ``https://ghcr.io/v2/`` response, and `/etc/resolv.conf`).

Every `ostree-rebase` cell passes `--accept-cross-base`. Since #191, an
unscannable target causes a refusal. The harness must opt in explicitly.
The cells assert the refusal first without the flag to test the gate.

### The matrix already has a cross-base pair

With the scan active, the test evaluated `is_cross_base`. The pair
`bluefin:stable → dakota:stable` is cross-base. The output from the ostree
re-base cell shows:

```
=== Cross-base UID/GID remap report ===
Diverging system accounts (renumbered during the re-base):
  wheel                    gid 10 -> 997
2 chown pass(es) will run over /var and preserved /etc.
Error: Cross-base re-base detected (host and target disagree on ID/ID_LIKE).
```

This corrects an earlier assumption. These pairs were called "same-lineage Fedora".
CentOS declares `ID_LIKE="rhel fedora"`. The existing pair qualifies as cross-base.

Issue #187 does not need a dedicated matrix cell. The gating `ostree-rebase` cell
asserts that the remap report appears after `--accept-cross-base`.
`E2E_CROSS_BASE=1` remains available for deliberate checks.

### Desktop migration (`E2E_DE_MIGRATE=1`)

The `bluefin -> aurora` cell (GNOME → KDE) passes `--de-migrate`.
Without this flag, the controller reported a skip and stash code never ran.

The harness will write a GNOME config for the first human user. It asserts that
a cross-desktop plan exists and `~/.local/share/de-migrate` exists.
The failure branches report the exact cause if a test fails.

Because desktop detection scans the target image, this cell depends on the
same registry path as `E2E_CROSS_BASE`.

After reboot, the cell checks the return trip. On the booted Aurora system,
it restores the GNOME stash with `de-migrate restore`. It asserts that the
seeded config is in `$HOME` and gone from the stash. The health check needs SDDM.

### tunaOS desktop ring

Four non-gating cells re-base between Albacore (AlmaLinux 10) desktop tags in a ring:
GNOME → Niri → COSMIC → XFCE → GNOME. Each desktop serves as source once and
target once. The tags use different display managers: GDM for GNOME, greetd with
DMS for Niri, cosmic-greeter for COSMIC, and greetd with gtkgreet for XFCE.

The ring does not use Yellowfin. On Kitten 10, a fresh install cannot start
D-Bus: SELinux denies `dbus_contexts` to dbus-broker (tunaOS#2485).
The XFCE cell forwards the journal to the serial console to record `qemu.log`.

Each cell sets `de_from` to the base desktop. The harness will write one config
file and asserts that `--de-migrate` stashes it. After reboot, it restores
the file and checks the target display manager.

### Post-reboot health check (every mode)

The per-mode assertions verify that user data survived. They do not prove that
the host system functions. Issue #267 showed D-Bus failures on Dakota → Utah migrations.

After every reboot into a migrated system, `tests/e2e-health.sh` runs inside the VM:

- The boot must complete with a valid state (`running` or `degraded`).
- No unit must fail, except patterns in `E2E_ALLOWED_FAILED_UNITS`.
- The system bus and logind must answer.
- For graphical targets, `graphical.target` must be active and the display
  manager must run.
- Every user and group in the target's `sysusers.d` must resolve.
- On SELinux targets, no denial against `unlabeled_t` files must occur.
  `restorecon -n` must find no new paths to relabel under `/etc` or `/var/home`.
  The harness records existing mislabeled paths before migration.
  Cells can list globs in `allowed_mislabeled` with a reason.

In composefs migration mode, the test checks all target `/etc` files.
Every file shipped by the target in `/etc` must exist after migration.
`tests/run-e2e.sh` lists allowed removals with reasons.

### Boot entries (`E2E_BOOT_ENTRIES=1`)

The gating `bluefin ostree re-base` cell runs `boot-entries`. It executes a
`--json` audit, then `--rename-branding --apply --yes`, followed by `--undo`.
It asserts that `efibootmgr -v` output is identical before and after.

This cell changes real UEFI NVRAM (#189). It tests the write path and the
snapshot-restore path against firmware variables.

`migrate-bootloader` is not yet implemented (issue #65).

Planned items per milestone:

- **M1**: dakota → utah (`ImageSwap`, `E2E_MODE=image-swap`) as a non-gating cell
- **M2**: ostree-rebase cell with `--bootloader systemd-boot` and kernel update test
- **M3**: cross-base cell for fedora → centos (#187)
- **M4**: migration without legacy-CLI bootc (`NativeStore` writer)
- **M0**: rollback cell with health check verification (#22/#26)

Design rules: create new cells for new capabilities.
Use `E2E_MODE` flags in the shared harness. Run cells locally with `just e2e*`.

## TUI testing (three layers)

Interactive code splits into three layers:

1. **State machines and headless display**: pure functions (`handle_key`)
   process keys. Every frame draws into ratatui's `TestBackend` for tests.
   `cargo test` runs these tests without a terminal.
2. **Terminal event loop in VM**: `tests/tui-e2e-driver.py` spawns the TUI on a
   pty and sends key events. The `tui-migrate` cell drives `etc-drift --interactive`
   and a full migration. The driver writes asciicast files (`--record`) and
   screenshots (`--snapshot-dir`). CI renders casts into GIFs with `agg` and
   saves them in `tui-walkthrough`. Replay casts locally with `asciinema play`
   or render with `agg`.
3. **Manual exploratory tests**: use Corral VMs (AGENTS.md) for UI tests.

Run driver self-tests with:

```bash
cargo build
python3 tests/tui-e2e-driver.py --mode wizard-expect-failure \
  --binary target/debug/bootc-migrate --target-image quay.io/x/y:z
```

## KVM runner options

The E2E matrix needs `/dev/kvm`. GitHub-hosted Linux runners provide KVM access.
`ubuntu-latest` is the default runner.

Both workflows set up podman storage. The runner image defaults to
`Native Overlay Diff: "false"`, which causes slow layer builds. The step
`Use native overlay diffs for podman` configures `/etc/containers/storage.conf`
to reduce build time to ~8 minutes. Self-hosted runners skip this step.

Runner precedence:

1. **`E2E_RUNSON_SPEC` set**: RunsOn EC2 runners with nested virtualization.
2. **`E2E_SELF_HOSTED=true`**: self-hosted runner with KVM.
3. **Default**: `ubuntu-latest`.

Use `KVM_E2E_ENABLED='false'` to disable the E2E matrix.

### Timeouts

| Budget | Value | Reason |
|---|---|---|
| Per-cell job timeout | 90 min | The tui-migrate cell takes 65 min end to end. |
| `e2e-gate` wait window (ci.yml) | 100 min | Covers job timeout and queue wait times. |

The `Enable KVM access` step checks `/dev/kvm` and warns if KVM is absent.

## Narrow dispatch

`e2e-single.yml` runs a single matrix cell:

```bash
gh workflow run e2e-single.yml -f filesystem=btrfs -f mode=composefs-migrate
```

This tests one scenario without the full matrix.

## Coverage floor

CI runs `cargo llvm-cov --workspace --all-features` with `--fail-under-lines`.
The floor prevents unintended drops in test coverage. Run `just coverage` locally.

## Failure triage

E2E workflows write failure summaries to `$GITHUB_STEP_SUMMARY`.
The summary contains the last phase banner, failed assertions, and log tails.

## Upstream drift canary

`.github/workflows/upstream-drift-canary.yml` runs twice weekly.
`tests/drift-canary.sh` checks images in `tests/canary-baseline.tsv` for CLI changes.
If the legacy builder changes to new-gen, the workflow creates an alert issue.
Update `canary-baseline.tsv` after you adopt upstream changes.

## Flake policy and notes

- Network timeouts and pull errors are infrastructure flakes. Rerun via narrow dispatch.
  Assertion failures need a diagnosis before a rerun.
- `gh run rerun` reuses the original merge commit. Merge main into the branch
  to test new fixes.
- Cancelled runs show as failed checks.
- Disk sizing affects guest behavior. A lack of disk space creates false errors.
- Mount errors on new-gen hosts are expected in phases 4 and 5 until PR #76 lands.
- The LVM-on-LUKS cell uses tight disk space (60 GB). Check for disk space
  exhaustion before you diagnose regressions.

### Incident: required-checks gate configuration

PR #73 merged automatically, but one E2E cell had failed because of disk exhaustion.
`required-checks` omitted `e2e-gate` from its `needs` list.
The fix added `e2e-gate` to `required-checks`.
All CI gates must feed into `required-checks`.

## Local reproduction

- `just check` runs validation and feature matrix tests.
- Run full scenarios locally with `just e2e` and `just e2e-lts`.
- `just drift-canary` runs upstream checks locally.
- `docs/cfs-cli-generations.md` lists commands for store reproduction tests.

## Future automation

1. Store integration tests in CI on loopback filesystems.
2. Scheduled version checks for composefs-rs releases.
3. Merge queue configuration with `required-checks` as the required context.
