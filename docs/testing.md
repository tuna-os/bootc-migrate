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
| E2E (VM) | Full migrations in QEMU: partition → install → migrate → reboot → assert | e2e-tests.yml, called from ci.yml's `e2e` job: a round-robin slice of the gating cells per PR/push, the full matrix nightly | ~30 min/cell |

Rules of thumb:

- **Logic goes down the pyramid.** Test behavior without a VM whenever possible.
  Extract a pure function, fabricate store contents through public APIs, or
  feed fixture bytes. E2E cells catch what only a
  real boot can: initrd behavior, bootloader handoff, mount namespaces,
  systemd unit order.
- **A failed E2E cell must name its phase.** The harness (`tests/run-e2e.sh`)
  prints `=== Phase N ===` banners and asserts with `FAIL:` prefixes so logs
  are greppable (`just e2e-failures`).
- **Evidence over inference.** When upstream behavior is unclear, test it
  with a loopback verity store or container probes. Do this before you design
  around an assumption. See the worked example in docs/cfs-cli-generations.md.

## E2E matrix: current and planned

Current (the four **MVP gates for regressions, which must stay**, plus the M1 addition):

| Cell | Proves | Status |
|---|---|---|
| bluefin stable → dakota (btrfs) | new-gen host path: probe → delegation ladder → builder | active |
| bluefin LTS → dakota (ext4) | legacy fast path, ext4 fs-verity | active |
| bluefin LTS → dakota (xfs+LUKS) | loopback store, passphrase injection | active |
| bluefin LTS → dakota (xfs+LVM+LUKS, split /var) | worst-case storage stack | active |
| bluefin LTS → dakota (xfs+LUKS, `bootloader-trial`) | GRUB2 source → systemd-boot one-boot trial, OVMF `BootOrder` swap, kernel-install ESP resync, guarded promotion and undo | added for #382; pending first green run |
| bluefin stable → bluefin gts (ostree-rebase mode) | OstreeDeploy strategy + rollback presence | PR #69/#70 |
| bluefin stable → aurora (ostree-rebase mode) | cross-DE native `/etc` merge probe (#80) | active, non-gating |
| bluefin stable → dakota (tui-migrate mode) | TUI wizard + Config Drift Review event loops on a pty (`tests/tui-e2e-driver.py`), then the full composefs pipeline + all default-mode assertions | active, gating |
| dakota stable (composefs-native) → fedora-bootc 44 (`composefs-to-ostree` mode) | the reverse backend switch (#260): `--plan` resolves `OstreeInstall`, the target's `bootc install to-existing-root` runs alongside, `/etc` + `/var` + `/var/home` fixtures survive, the reboot lands in the OSTree deployment, and the composefs "Linux Boot Manager" entry and ESP kernel remain as rollback. Then `bootc-rebase commit` (#315) runs as a dry run that deletes nothing, and with `--apply` it removes `/sysroot/state` and `/sysroot/composefs` but keeps the OSTree repo, the deployment and the forensics; a reboot follows, then the health check | active, non-gating |
| dakota stable (composefs-native) → utah testing (`composefs-to-ostree` mode) | the Dakota → Utah migration through `OstreeInstall` (#302): the same assertions as the fedora-bootc cell, plus the shim/GRUB entry first in `BootOrder`, the booted os-release `ID=hummingbird` and GDM running | active, non-gating |
| dakota stable (composefs-native) → utah testing (`image-swap` mode) | a composefs image swap across distributions: `--plan` resolves `ImageSwap`, the host's `bootc switch` stages Utah (Bluefin on Fedora Hummingbird), `/etc` + `/var` + `/var/home` fixtures survive, the reboot lands in Utah and the Dakota deployment stays as rollback | active, non-gating |
| tunaOS marlin gnome (composefs-native, Arch) → utah testing (`image-swap` mode, `E2E_CROSS_FAMILY=1`) | a cross-family image swap: the swap is refused without `--accept-cross-base`, then the host's `bootc switch` stages Utah, and the same fixture, reboot and rollback checks as the Dakota cell run | active, non-gating |
| tunaOS marlin gnome (composefs-native, Arch) → utah testing (`composefs-to-ostree` mode) | the OstreeInstall route from a host with no `efibootmgr` and no SELinux: NVRAM through the target's `efibootmgr`, `/boot` labelled with Utah's policy, the apps Marlin preinstalled still installed after Utah's first-boot `flatpak preinstall`, and the commit dry run's `/var` check | active, non-gating |
| bluefin stable → bootcrew/opensuse-bootc (`composefs-migrate`, `E2E_CROSS_FAMILY=1`) | cross-family gate and `/etc` policy: target defaults, sidecars, machine state and identity merge | active, non-gating |
| bluefin stable → bootcrew/opensuse-bootc (`ostree-rebase`, `E2E_CROSS_FAMILY=1`) | native finalization, then cross-family policy, `/var` remap and SELinux relabel | active, non-gating |
| bluefin stable → bootcrew/debian-bootc (`E2E_CROSS_FAMILY=1`) | cross-family migration to Debian (apt package family, `/etc/apt`, `/etc/dpkg`) | active, non-gating |
| bluefin stable → bootcrew/ubuntu-bootc (`E2E_CROSS_FAMILY=1`) | cross-family migration to Ubuntu (`ID_LIKE=debian`) | active, non-gating |
| bluefin stable → bootcrew/arch-bootc (`E2E_CROSS_FAMILY=1`) | cross-family migration to Arch Linux (pacman package family, no `ID_LIKE`) | active, non-gating |
| Fedora Cloud → `fedora-bootc:43` (`package-install`) | official Fedora Cloud btrfs image → package host → OSTree target, `/home` + `/var` fixtures and Previous system | exploratory (#380) |
| Fedora Workstation → Bluefin (`package-install`) | Fedora Workstation package layout with separate `/boot`, GNOME/GDM, desktop state and Flatpak fixtures | exploratory (#380) |
| Ubuntu Cloud → `fedora-bootc:43` (`package-install`) | official Ubuntu ext4/dpkg cloud image, `--accept-cross-base`, and cross-family `/etc` policy | exploratory (#380) |

### Package-based base images (`E2E_MODE=package-install`)

The package cells do not run `bootc install to-disk`. `tests/package-base.sh`
downloads the official cloud image at its pinned version, verifies its SHA-256, converts it
to `disk.raw`, grows the root filesystem, and attaches a `cloud-localds` seed.
The seed installs the root SSH key and the harness socket before the live
fixture step. `fedora-workstation` starts from the Fedora Cloud image and installs
the Workstation environment with `dnf`. It creates a separate ext4 `/boot`
partition. Thus, the test covers a bootloader layout beyond a single btrfs root.

Select the source with `E2E_PACKAGE_BASE=fedora-cloud`,
`fedora-workstation`, or `ubuntu-cloud`. The Ubuntu cell also sets
`E2E_PACKAGE_CROSS_FAMILY=1`, which adds `--accept-cross-base` to the
package → OSTree command. The matrix skips the base-image `podman pull` for
these cells but still pulls the target image used by the migration.

Each package cell creates and checks these fixtures:
- A real human password, an SSH key and a `/home` marker.
- Modified configuration for the package manager and an unpackaged `/etc` file.
- A NetworkManager profile and a rootless Podman image.
- System and user Flatpak fixtures, and the package-only `cowsay` CLI.

After the target reboot, it checks `bootc status`, health, the software report
and precedence for the target vendor's `/etc` files. Then it selects the
generated `Previous system` entry, verifies the old distribution and data,
and returns to the target.
The three rows remain exploratory until the package route (#373–#379) is complete
and the first run passes. At that point, make the Fedora Cloud row a required gate.

### Cross-base OstreeDeploy coverage — active, gating

The gating `bluefin:stable → dakota:stable` OstreeDeploy cell is cross-base:
the target scan finds different `ID`/`ID_LIKE` lineages. It exercises the
complete #67 flow without adding another expensive matrix entry:

1. A dry run without `--accept-cross-base` must identify the pair as
   cross-base and refuse it. An unscannable target instead takes #191's
   fail-closed `Unknown` path, which the harness reports separately.
2. The harness finds the first UID/GID difference in the remap report. It
   creates a file in the stateroot's shared `/var`. The file uses the old
   numeric owner. It also adds a marker to `/etc/dconf/db/distro`. Bluefin
   and Dakota have different vendor defaults for this file. This creates an
   intentional three-way conflict.
3. The accepted re-base must emit the remap and `/etc` conflict reports.
   It must also emit their JSON forms. Before reboot, the `/var` fixture must
   have the target ID. `/etc/dconf/db/distro` must equal the target default.
   The `.rebase-old` file must contain the displaced marked value.
4. The same ownership, target-default and sidecar checks run again after
   reboot.

These assertions prevent two vacuous results that before left #67 unproven.
One result is a scan that never evaluates `is_cross_base`. The other is a
report without work from either filesystem pass. The assertions also cover
the actual OSTree `/var` layout. Deployments share
`/ostree/deploy/<stateroot>/var`; no `var` tree exists per deployment.

Every ostree-rebase cell probes the registry path in the guest
(`curl`, `https://ghcr.io/v2/`, and resolver state) before it starts the gate.
This separates registry or token infrastructure from code and assertion
failures. Current runs no longer get the earlier ghcr.io token-request 403.
They receive the expected registry challenge. They scan the target and
classify the Bluefin → Dakota pair as cross-base.

`E2E_CROSS_BASE=1` can make an ostree-rebase pair reach the cross-base
report. The normal gating cell no longer needs that flag. Its dry-run scan
identifies a cross-base pair. The harness then runs all remap and policy
checks for conflicts.

### Desktop migration (`E2E_DE_MIGRATE=1`)

The `bluefin -> aurora` cell (GNOME → KDE) passes `--de-migrate`.
Without this flag, the controller reported a skip and stash code never ran.

The harness will write a GNOME config for the first human user. It asserts that
a cross-desktop plan exists and `~/.local/share/de-migrate` exists.
The failure branches report the exact cause if a test fails.

Desktop detection uses the same target-image scan as the cross-base gate.
The fix for registry token scope keeps both checks live. The harness now
separates a scan regression from a same-desktop result.

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

The separate `bootloader-trial` cell covers #65/#382. It installs the
standalone systemd-boot path on a GRUB2 Bluefin guest. OVMF does not honor
`BootNext`, so the cell saves and swaps `BootOrder`. It reboots through the

trial and calls the installed `kernel-install` resync hook for a second kernel
version. It then tests `--promote` and `--undo`. The final check confirms the
original firmware order and GRUB fallback. It also confirms restoration of
migration-owned ESP files and the hook.

Planned items per milestone:

- **M1**: dakota → utah (`ImageSwap`, `E2E_MODE=image-swap`), active as the non-gating cell above
- **M3**: the cross-base cell above covers centos-family → fedora-family.
  The exit criterion names the opposite direction (fedora → centos). That
  direction needs a target image from the CentOS family. The harness does not
  install one now. Decide if the direction matters (#187).
- **M4**: a migration where **no** legacy-CLI bootc exists (NativeStore
  writer); kernel-version gate ≥6.12 for file-backed EROFS mounts
- **M0**: rollback cell — migrate, boot, and run `rollback`. Confirm that the
  OSTree deployment boots and the store stays intact (#22/#26). Confirm that
  health scripts compatible with greenboot are present.

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

The workflows add the RunsOn key `runs-on=<run-id>`.
You can tune other options and leave the workflow YAML unchanged.
Keep these constraints:

- `nested-virt` needs an x64 image from a supported family (`c8i`, `m8i`, or `r8i`).
- The `volume=120gb` setting covers the 60G disk for the guest, two ~5 GB images, and the Rust target.
- Use `spot=false`, or `retry=when-interrupted`, because spot interruption is risky for a 30–90-minute cell.
2. **`E2E_SELF_HOSTED=true`** → the `kvm`-labelled self-hosted host
   (kanpur), the original setup.
3. **Neither** → `ubuntu-latest`.

Set `KVM_E2E_ENABLED='false'` to stop the matrix.
The `required-checks` job treats a skipped `e2e` job as a pass.
Leave the variable unset or set it to `'true'` to run the matrix.
Change the variable, not the workflow.

### Which cells run when

The scenarios live in `.github/e2e-scenarios.yml`. Cells without
`allow_failure: true` are gating. Pull requests and pushes to `main` keep
`E2E_ROUND_ROBIN_SIZE` baseline cells (default 2). The first cell is
`(run_number × size) mod <gating cells>`.

TUI source, entry point, drift review, harness, dependency, or selector changes
also need the existing `tui-migrate` cell. The path list is in
`scripts/select_e2e.py`. This adds at most one VM and keeps every baseline cell.
PRs compare the merge base with the head. Pushes compare `before` with `after`.

A failed diff stops the plan. Tests run with `just test-ci-selection`.
The nightly schedule and `workflow_dispatch` run every cell.

### Timeouts, and why they are what they are

| Budget | Value | Reason |
|---|---|---|
| Per-cell job timeout | 90 min | The tui-migrate cell measured 65 min end to end (image bake + migration + reboot + rollback + commit, driven through the wizard). |
| `e2e` job (ci.yml) | — | No wait window: ci.yml calls e2e-tests.yml as a reusable workflow, so `required-checks` depends on the matrix result directly. The old `e2e-gate` polled for a separately triggered run and held a runner for up to five hours per PR. |

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

`.github/workflows/upstream-drift-canary.yml` runs twice a week and on dispatch.
`tests/drift-canary.sh` probes every image in `tests/canary-baseline.tsv`.
It checks the cfs CLI generation. Exit 1 means drift and creates or updates a
canary issue. Exit 2 means that probe infrastructure failed. The next run tries

again without an alert. This catches breakage from #72.

**A change from the pinned legacy builder to the new generation is the important
alert.** It breaks the delegation ladder and blocks the MVP.

To maintain the baseline: when a drift is real and absorbed (code adapted,
docs updated), update `canary-baseline.tsv` in the same PR that absorbs it.

- Network timeouts and pull errors are infrastructure flakes. Rerun via narrow dispatch.
  Assertion failures need a diagnosis before a rerun.
- `gh run rerun` reuses the original merge commit. Merge main into the branch
  to test new fixes.
- Cancelled runs show as failed checks.
- Disk sizing affects guest behavior. A lack of disk space creates false errors.
- Mount errors on new-gen hosts are expected in phases 4 and 5 until PR #76 lands.
- The LVM-on-LUKS cell uses tight disk space (60 GB). Check for disk space
  exhaustion before you diagnose regressions.

- **Retry taxonomy:** Retry a cell after a GHCR connection reset or ENOSPC
  during an image pull. Diagnose assertion failures and phase errors before a retry.
- **`gh run rerun` uses the original merge commit.** If main changed,
  merge main into the PR branch to test the new code.
- **Cancelled runs show `fail`** in `gh pr checks`. A stuck queue can need
  cancellation and a new run.
- **Disk size is part of the test.** ENOSPC inside the guest can cause errors
  later in the run. See the probe asymmetry note in CONTEXT.md.
  The cells explain their disk sizes in e2e-tests.yml.
- **Mount errors on new-gen hosts:** Phases 4/5 use podman fallbacks until
  PR #76 lands. Do not treat those expected mount errors as the failure signal.
- **The LVM-on-LUKS cell has the least free space.** Its disk is 60G. It
  contains a BIOS-boot partition, an ESP, separate `/boot`, and a fixed 4G
  `/var` LV before root. Check for ENOSPC first. Then investigate other regressions.

### Incident: `required-checks` did not gate on `e2e-gate`

PR #73 auto-merged on 2026-07-19. Its LVM-on-LUKS cell had an ENOSPC failure.
Branch protection watched `required-checks`. That job listed `[validate,
cargo-deny, coverage, actionlint]` in `needs`, but omitted `e2e-gate`.
The two jobs had been independent since the workflow began.
GitHub accepted the fast checks even when E2E failed.

The fix added `e2e-gate` to `required-checks` in `needs`.
Every gate must feed into `required-checks`; branch protection cannot see a
new job that this list omits.
The merge needed no revert: the other three cells verified the fix, including
btrfs. The failed cell had the ENOSPC flake from the previous section.

## Local reproduction

- `just check` runs the checks from CI's validate job, including
  `test-all-features`.
- `just e2e`, `e2e-lts`, `e2e-luks`, and `e2e-lvm` run full cells locally.
  Use `e2e-status`, `e2e-ssh`, `e2e-tail`, `e2e-scan`, or `e2e-reboot-test`
  to inspect one phase.
- `just drift-canary` runs the upstream probe without CI.
- For cross-generation store experiments, see docs/cfs-cli-generations.md.
  These need a loopback filesystem made with `mkfs.ext4 -O verity`.
  Root filesystems often lack verity.

## Future automation

1. Add a weekly integration test for the store: write with the legacy CLI,
   then run new-gen fsck. This needs about 2 GB and no VM.
2. Watch `cargo update --dry-run` nightly for new composefs-rs 0.x releases.
   NativeStore pins 0.7.
3. After the stacked fleet lands, enable the merge queue on GitHub.
   Use `required-checks` as the single required context; it accepts optional jobs that did not run.

## E2E input regressions

Run `just test-harness` for the cloud-init package list, package-manager fixture,
and executable setup for bootloader trials. `just check` also runs these tests without
a VM. The tests execute the shell snippets that produce the inputs; they do not
prove that the scenario boots.

[Run 39227338849](https://github.com/tuna-os/bootc-migrate/actions/runs/39227338849)
failed before migration in three harness paths:

- Fedora package install appended a bare marker to `dnf.conf`; `dnf` rejected
  line 4 with `Missing '='`. Use a comment marker and retain the sidecar check.
- Ubuntu cloud-init requested `NetworkManager`, the Fedora package name.
  Ubuntu needs `network-manager`.
- The bootloader trial wrote to `/usr/local/bin`, which was read-only. The remote
  shell continued after failed `install` commands. Keep binaries in a unique root-owned `0700` directory under `/var/tmp`.
  Reject symlinks and wrong ownership, check executable bits, and stop if setup fails.
  Stage, promote, and undo use that same directory across the trial reboot.

The remaining failures in that run (read-only stateroot `/var`, failed service
health, and SSH readiness) need separate diagnosis. These input fixes do not
waive those checks or establish that the full migration passes.
