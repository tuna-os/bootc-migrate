# Testing & automation

How this project stays trustworthy: what runs where, what gates what, and
what automation does so humans don't have to remember it. Companion to
ROADMAP.md (the project plan) and docs/cfs-cli-generations.md (the
experiments that informed this strategy).

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
| bluefin stable → bootcrew/opensuse-bootc (`E2E_CROSS_FAMILY=1`) | the cross-family gate refuses without `--accept-cross-base`; with it, the cross-family `/etc` policy (#256): target defaults win, `.rebase-old` sidecars, carried machine state, target-first identity merge, first-boot unit | active, non-gating |
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
2. The harness takes the first UID/GID divergence from the remap report and
   creates a file under the stateroot's shared `/var` with that old numeric
   owner. It also appends a marker to `/etc/dconf/db/distro`, whose Bluefin
   and Dakota vendor defaults differ, producing a deliberate three-way
   conflict.
3. The accepted re-base must emit both the remap and `/etc` conflict reports
   and their JSON twins. Before reboot, the `/var` fixture must have the
   target ID; `/etc/dconf/db/distro` must equal the target default; and the
   displaced marked value must exist as `.rebase-old`.
4. The same ownership, target-default and sidecar checks run again after
   reboot.

These assertions prevent both vacuous-pass shapes that originally left #67
unproven: a scan that never evaluates `is_cross_base`, and reports that render
without either filesystem apply pass doing work. They also cover OSTree's
actual `/var` layout: deployments share the physical
`/ostree/deploy/<stateroot>/var`; there is no per-deployment `var` tree to
walk.

Every ostree-rebase cell still probes the guest's registry path
(`curl`, `https://ghcr.io/v2/`, and resolver state) before running the gate.
This distinguishes registry/token infrastructure from a code or assertion
failure. The earlier ghcr.io token-request 403 is fixed: current runs receive
the expected registry challenge, scan the target, and classify the
Bluefin → Dakota pair as cross-base.

`E2E_CROSS_BASE=1` remains available to require a deliberately selected
ostree-rebase pair to reach the cross-base report. The normal gating cell no
longer needs that flag: once its dry-run scan identifies a cross-base pair,
the harness automatically runs all remap and conflict-policy assertions.

### Desktop migration (`E2E_DE_MIGRATE=1`) — active on the cross-DE cell

The `bluefin -> aurora` cell (GNOME → KDE) now passes `--de-migrate`. Before
this, no cell passed it, so the controller reported "skipped (--de-migrate not
passed)" and #68's stash/restore code never ran — the cell exercised the
re-base route while proving nothing about desktop migration.

The harness seeds a GNOME config for the first human account, then asserts
both that a cross-desktop plan was reported *and* that
`~/.local/share/de-migrate` exists afterwards. The second half matters: a plan
without a stash is precisely #68's unshipped half. Like the cross-base
assertion, the failure branches name which of the four silent causes occurred
(flag not wired, same desktop, no human users, or target scan failed) rather
than guessing one.

Desktop detection uses the same target-image scan as the cross-base gate.
The registry token-scope fix therefore keeps both checks live; a scan
regression is distinguished explicitly from a same-desktop result.

After the reboot, the cell also checks the return trip. On the booted Aurora,
it restores the GNOME stash with the `de-migrate restore` subcommand. Then it
asserts that the seeded GNOME config is back in `$HOME` and gone from the
stash. The shared health check below also requires SDDM, not GDM, as the
display manager.

### tunaOS desktop ring

Four non-gating cells re-base between the Albacore (AlmaLinux 10) desktop
tags in a ring:
GNOME → Niri → COSMIC → XFCE → GNOME. Each desktop is a source once and a
target once. Each change of display manager runs once, because the tags use
different ones: GDM for GNOME, greetd with the DMS greeter for Niri,
cosmic-greeter for COSMIC, and greetd with gtkgreet for XFCE.

The ring does not use Yellowfin. On its rolling Kitten 10 base, a fresh
install cannot start D-Bus: SELinux denies dbus-broker its
`dbus_contexts` file (tunaOS#2485). Every Yellowfin base therefore fails
before the migration starts. The XFCE cell forwards the journal to the
serial console, so an EL10 base with this problem shows the reason in
`qemu.log`.

Each cell sets `de_from` to the base desktop. The harness seeds one config
file of that desktop and asserts that `--de-migrate` stashes it. After the
reboot, it restores the file and requires the target's display manager.

### Post-reboot health check (every mode)

The per-mode assertions prove that user data survived. They do not prove that
the system which carries the data works. The Dakota → Utah bugs (#267) broke
D-Bus and left SSH working only by chance.

After every reboot into a migrated system, `tests/e2e-health.sh` runs inside
the VM and fails the cell when any of these is not true:

- The boot completed. The system state is "running" or "degraded".
- No unit failed, other than the globs in `E2E_ALLOWED_FAILED_UNITS`. Each
  entry needs a reason next to it.
- The system bus and logind answer.
- When the default target is graphical, `graphical.target` is active and the
  display manager runs. It must be the cell's `expect_dm` when set (`gdm`,
  `sddm`).
- Every user and group in the target's `sysusers.d` resolves.
- On an enforcing SELinux target: no denial against an `unlabeled_t` file,
  and `restorecon -n` finds no new path to relabel under `/etc` or
  `/var/home`. Just before the migration, the harness records the paths
  that the base already has mislabeled. Those paths are reported but do
  not fail the cell. For example, tunaOS labels `/var/home/linuxbrew`
  `home_root_t` on a fresh install.
  A cell can also list path globs in `allowed_mislabeled` when the image's
  own policy is not consistent. The cell must give the reason. The
  `albacore:gnome` policy maps `/var/home` to `/home` but writes its home
  contexts for `/var/home`, so the two ring cells that include it allow
  `/var/home/*`.

The composefs migration mode also asserts the other direction of the
fresh-image comparison. Every file that the target image ships in `/etc` must
exist after the migration. The list of paths that the migration removes on
purpose is in `tests/run-e2e.sh`, with a reason for each.

### Boot entries (`E2E_BOOT_ENTRIES=1`) — live NVRAM coverage

The gating `bluefin ostree re-base` cell now runs `boot-entries`: a read-only
`--json` audit, then a `--rename-branding --apply --yes` followed by `--undo`,
asserting that `efibootmgr -v` output is byte-identical before and after.

This is the first cell to mutate real UEFI NVRAM (#189). Until it existed,
#31's `efibootmgr` executor had only unit tests of the *plan*; the write path
and the snapshot-restore path had never run against real firmware variables.

The separate `bootloader-trial` cell covers #65/#382. It installs the
standalone systemd-boot path on a GRUB2 Bluefin guest, saves and swaps
`BootOrder` because OVMF does not honor `BootNext`, reboots through the trial,
calls the installed `kernel-install` resync hook for a second kernel version,
then exercises `--promote` and `--undo`. The final assertion requires the
original firmware order, GRUB fallback, migration-owned ESP files and hook to
be restored.

Planned, one per milestone exit (see ROADMAP.md):

- **M1**: dakota → utah (`ImageSwap`, `E2E_MODE=image-swap`), active as the non-gating cell above
- **M3**: the cross-base cell above covers centos-family → fedora-family.
  The exit criterion names the *other* direction (fedora → centos), which
  needs a CentOS-family target image the harness does not currently install;
  decide explicitly whether direction matters (#187)
- **M4**: a migration where **no** legacy-CLI bootc exists (NativeStore
  writer); kernel-version gate ≥6.12 for file-backed EROFS mounts
- **M0**: rollback cell — migrate, boot, `rollback`, assert the OSTree
  deployment boots and the store is intact (#22/#26); greenboot-compatible
  health scripts asserted present

Cell design rules: new capability ⇒ new cell (never widen an MVP cell);
prefer `E2E_MODE` branches in one harness over new harnesses; every cell
must be runnable locally (`just e2e*` with env overrides).

## TUI testing (three layers)

Interactive code splits the same way the rest of the project does —
pure logic proven cheap, live behavior proven on a real system:

1. **State machines + rendering, headless**: every checklist/wizard's key
   handling is a pure function (`handle_key`) and every frame draws into
   ratatui's `TestBackend` for content assertions — `tui::tests` and
   `drift_review::tests` in `bootc-migrate`, `boot_entry_review::tests`
   in `bootc-rebase`. Runs in `cargo test`, no terminal involved.
2. **The raw terminal event loop, in the VM**: `tests/tui-e2e-driver.py`
   (stdlib-only python3, runs on the system under test) spawns the TUI on
   a pty, reconstructs the screen from the emitted escape sequences, and
   types like a human. The `tui-migrate` cell uses it to drive
   `etc-drift --interactive` and then a full migration through the
   wizard; two hard-won rules live in its docstring — match against a
   grid, never the raw stream (ratatui diff-draws), and never type while
   a forced-repaint winsize nudge is in flight (the key gets dropped).

   The cell also documents itself: the driver records each flow as an
   asciicast v2 file (`--record`) and saves a plain-text screenshot of
   every wizard screen it reaches (`--snapshot-dir`). CI renders the
   casts into timelapse GIFs with `agg` (long quiet phases collapsed via
   `--idle-time-limit`) and publishes casts + GIFs + screenshots as the
   `tui-walkthrough` artifact on every tui-migrate run, pass or fail —
   the automated successor to the manual vhs capture in
   `scripts/capture-screenshots.sh`. Replay a cast locally with
   `asciinema play tui-migrate.cast` or re-render with
   `agg tui-migrate.cast out.gif`.
3. **Exploratory, by hand**: Corral VMs (AGENTS.md), for anything the
   scripted flow doesn't reach (resize behavior, colors, feel).

The driver self-tests on any non-OSTree dev box — the wizard runs to the
Failed screen and must exit cleanly:

    cargo build
    python3 tests/tui-e2e-driver.py --mode wizard-expect-failure \
      --binary target/debug/bootc-migrate --target-image quay.io/x/y:z

## KVM runner options

The whole E2E matrix needs `/dev/kvm` (TCG is ~10× slower and blows any
sane timeout). **GitHub-hosted Linux runners provide it** — measured
2026-08-27, `crw-rw-rw- root:kvm`, guest SSH in 31 seconds — so
`ubuntu-latest` is the default and no variable is needed to turn the
matrix on. The long-standing "hosted runners have no KVM" note in these
workflows predated GitHub enabling it and was simply stale.

One hosted-runner adjustment is required and lives in both workflows:
the runner image's podman config leaves `Native Overlay Diff: "false"`,
which makes every `podman build` layer commit walk and compare the whole
~10 GB bluefin rootfs — about **30 minutes per trivial `RUN`**, enough
to burn a 45-minute budget before QEMU ever starts. The `Use native
overlay diffs for podman` step writes a minimal `/etc/containers/
storage.conf` and resets the still-empty graph, cutting the full image
bake to ~8 minutes. It is skipped when `E2E_SELF_HOSTED=true`, because
that host owns its storage config and its graph holds cached images.

Runner selection, highest precedence first:

1. **`E2E_RUNSON_SPEC` set** → [RunsOn](https://runs-on.com) ephemeral
   EC2 runners in your own AWS account (useful for more cores than a
   hosted runner's 4, or when hosted capacity is contended). Install the
   RunsOn GitHub App and deploy its CloudFormation stack **including the
   nested launch templates** (an existing stack must be upgraded before
   `nested-virt` jobs run — plain EC2 VMs have no nested virtualization,
   so this is what exposes `/dev/kvm` without paying for `.metal`), then
   set:

       E2E_RUNSON_SPEC=family=c8i+m8i+r8i/cpu=8/ram=32/volume=120gb/nested-virt/image=ubuntu24-full-x64/spot=false

   The workflows prepend the `runs-on=<run-id>` routing key RunsOn
   requires; everything after it is yours to tune without touching
   workflow YAML. Constraints worth keeping: `nested-virt` needs an x64
   image on a supported family (c8i/m8i/r8i); `volume=120gb` covers the
   60G sparse guest disk plus both ~5 GB images and the Rust target;
   `spot=false` (or `retry=when-interrupted`) because a 30-90-minute
   cell is a bad spot candidate.
2. **`E2E_SELF_HOSTED=true`** → the `kvm`-labelled self-hosted host
   (kanpur), the original setup.
3. **Neither** → `ubuntu-latest`.

`KVM_E2E_ENABLED` is now a **kill switch, not an enable switch**: set it
to `'false'` to stand the whole matrix down (`required-checks` treats the
skipped `e2e` job as a pass); unset or `'true'` both run. Switching lanes is
a variable change, never a workflow edit.

### Which cells run when

The scenarios live in `.github/e2e-scenarios.yml`. Cells without
`allow_failure: true` are gating. Pull requests and pushes to `main` keep
`E2E_ROUND_ROBIN_SIZE` baseline cells (default 2). The first cell is
`(run_number × size) mod <gating cells>`.

TUI source, entry point, drift review, harness, dependency, or selector changes
also require the existing `tui-migrate` cell. The path list is in
`scripts/select_e2e.py`. This adds at most one VM; it keeps every baseline cell.
PRs compare the merge base with the head. Pushes compare `before` with `after`.
A failed diff stops the plan. Tests run with `just test-ci-selection`.
The nightly schedule and `workflow_dispatch` run every cell.

### Timeouts, and why they are what they are

| Budget | Value | Reason |
|---|---|---|
| Per-cell job timeout | 90 min | The tui-migrate cell measured 65 min end to end (image bake + migration + reboot + rollback + commit, driven through the wizard). |
| `e2e` job (ci.yml) | — | No wait window: ci.yml calls e2e-tests.yml as a reusable workflow, so `required-checks` depends on the matrix result directly. The old `e2e-gate` polled for a separately triggered run and held a runner for up to five hours per PR. |

Measured cell runtimes on `ubuntu-latest` (2026-08-27): ostree-rebase-plan
26 min, composefs-migrate 36 min, tui-migrate 65 min. The `Enable KVM
access` step probes `/dev/kvm` and warns rather than failing, so a runner
without it degrades loudly — a cell that suddenly takes hours is that
warning going unread.

## Narrow dispatch (implemented)

`e2e-single.yml` dispatches exactly one cell with chosen parameters:

    gh workflow run e2e-single.yml -f filesystem=btrfs -f mode=composefs-migrate

One failing scenario gets iterated alone instead of burning the whole
matrix per attempt — the contract the ci-fix-loop practice expects. (A
matrix-filter `if` was rejected: the `matrix` context is not available in
job-level `if`, a bug actionlint caught before it shipped.) Keep its step
sequence in sync with e2e-tests.yml.

## Coverage floor (implemented)

CI's `coverage` job runs `cargo llvm-cov --workspace --all-features` with
a **regression floor** (`--fail-under-lines`, see `just coverage-check`)
and posts the summary to the job summary. The floor is not a target — it
exists to catch commits that delete or bypass meaningful coverage. Raise
it as milestones add tests; never lower it to merge. Local:
`just coverage` / `coverage-html`. Baseline 2026-07-19: ~27% lines, with
the pure-logic modules at 75–100% and the deliberately-untested layers
(process orchestration, TUI, network/filesystem effectors) at 0 — those
are the E2E cells' job.

## Failure triage (implemented)

Both E2E workflows write a triage block to `$GITHUB_STEP_SUMMARY` on
failure: last phase banner reached, every `FAIL:`/`ERROR:` assertion, and
the log tail — diagnosis starts from the annotation, not a log download.

## Upstream drift canary (implemented)

`.github/workflows/upstream-drift-canary.yml` runs twice weekly (and on
dispatch): `tests/drift-canary.sh` probes every image in
`tests/canary-baseline.tsv` for its cfs CLI generation and exits 1 on
drift (auto-filing/updating a canary issue), 2 on probe-infra failure
(retried next run, never alerted). The #72 breakage is exactly what this
catches; **the pinned legacy builder drifting to new-gen is the important
alert** — it breaks the delegation ladder and blocks the MVP.

To maintain the baseline: when a drift is real and absorbed (code adapted,
docs updated), update `canary-baseline.tsv` in the same PR that absorbs it.

## Flake policy & known gotchas

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
- **The LVM-on-LUKS cell has the least free space.** Its 60G disk contains
  BIOS-boot, ESP, separate /boot, and a fixed 4G /var LV before root.
  Check for ENOSPC first. Then investigate other regressions.

### Incident: `required-checks` did not gate on `e2e-gate`

PR #73 auto-merged on 2026-07-19. Its LVM-on-LUKS cell failed due to ENOSPC.
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

## Gaps / next automation (tracked, not yet built)

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
