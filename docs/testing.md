# Testing & automation

How this project stays trustworthy: what runs where, what gates what, and
what is automated so humans don't have to remember it. Companion to
ROADMAP.md (what we're building) and docs/cfs-cli-generations.md (the
evidence-gathering method this strategy grew out of).

## The pyramid

| Layer | What | Where it runs | Budget |
|---|---|---|---|
| Unit | Pure logic: parsers, planners, mergetc tables, routing, digest handling, probe classification | `just test` / CI `validate`, every push | seconds |
| Feature-matrix | Same, with every cargo feature on (`composefs-native` is invisible to the default build) | `just test-all-features` (part of `just check`) | seconds |
| Integration (host) | Real repositories on loopback filesystems, real podman probes of real images — no VM | developer machines + scheduled canary | minutes |
| E2E (VM) | Full migrations in QEMU: partition → install → migrate → reboot → assert | e2e-tests.yml matrix, PR-gated via `e2e-gate` | ~30 min/cell |

Rules of thumb:

- **Logic goes down the pyramid.** If a behavior can be tested without a VM
  (extract a pure function, fabricate store contents through public APIs,
  feed fixture bytes), it must be. The E2E cells exist to catch what only a
  real boot can: initrd behavior, bootloader handoff, mount namespaces,
  systemd ordering.
- **A failed E2E cell must name its phase.** The harness (`tests/run-e2e.sh`)
  prints `=== Phase N ===` banners and asserts with `FAIL:` prefixes so logs
  are greppable (`just e2e-failures`).
- **Evidence over inference.** When upstream behavior is in question, build
  the experiment (loopback verity store, container probes) before designing
  around an assumption — docs/cfs-cli-generations.md is the worked example.

## E2E matrix: current and planned

Current (the four **untouchable MVP regression gates** + M1 addition):

| Cell | Proves | Status |
|---|---|---|
| bluefin stable → dakota (btrfs) | new-gen host path: probe → delegation ladder → builder | active |
| bluefin LTS → dakota (ext4) | legacy fast path, ext4 fs-verity | active |
| bluefin LTS → dakota (xfs+LUKS) | loopback store, passphrase injection | active |
| bluefin LTS → dakota (xfs+LVM+LUKS, split /var) | worst-case storage stack | active |
| bluefin stable → bluefin gts (ostree-rebase mode) | OstreeDeploy strategy + rollback presence | PR #69/#70 |
| bluefin stable → aurora (ostree-rebase mode) | cross-DE native `/etc` merge probe (#80) | active, non-gating |
| bluefin stable → dakota (tui-migrate mode) | TUI wizard + Config Drift Review event loops on a pty (`tests/tui-e2e-driver.py`), then the full composefs pipeline + all default-mode assertions | active, gating |

### Cross-base mode (`E2E_CROSS_BASE=1`) — mechanism ready, blocked

`ostree-rebase` mode takes `E2E_CROSS_BASE=1`, which adds
`--accept-cross-base` (the route is refused without it) and asserts that
`=== Cross-base UID/GID remap report ===` appeared in the output.

The assertion is the point. `gate_cross_base` returns `None` and prints
nothing whenever it declines to act, so silence is indistinguishable from
success and an unasserted cell would pass vacuously.

**There is currently no matrix cell using it**, because running it surfaced a
blocker (#191): inside the E2E guest the target-image *scan* cannot reach
ghcr.io, so cross-base detection never runs. `bootc switch` itself pulls fine
in the same guest, so this is specific to the scan path. Until that is fixed,
no cell can demonstrate the cross-base code executing, and a permanently-red
cell would only add noise.

That same scan failure is why **every `ostree-rebase` cell now passes
`--accept-cross-base`**. Since #191 an unscannable target is a refusal rather
than a silent proceed, so the harness — which knows its image pairs are
same-lineage Fedora — has to opt in explicitly, exactly as a human operator
would. The cells assert the refusal *first*, without the flag, so the gate's
wiring has live coverage and cannot regress to waving things through; the
unit tests cover the policy, not the wiring.

The plumbing stays wired — `just e2e-cross-base` locally, and a `cross_base`
input on `e2e-single.yml` — so the cell can return as one matrix entry once
#191 clears. Tracked by #187.

Note also that the intended image pair may not qualify anyway: `is_cross_base`
is lineage-aware (`scan.rs`), returning false when either side's `ID_LIKE`
contains the other's `ID`. CentOS declares `ID_LIKE="rhel fedora"`, so
CentOS → Fedora is same-lineage **by design** — see the existing
`cross_base_same_family_via_id_like_is_clean` test. That question is still
open because the scan failure meant `is_cross_base` was never evaluated.

Planned, one per milestone exit (see ROADMAP.md):

- **M1**: dakota → dakota:other-tag (`ImageSwap`, `E2E_MODE=image-swap`)
- **M2**: ostree-rebase cell + `--bootloader systemd-boot` + simulated
  kernel update asserting ESP resync; `--undo` restores GRUB
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
to `'false'` to stand the whole matrix down (`e2e-gate` treats a skipped
run as a trivial pass); unset or `'true'` both run. Switching lanes is a
variable change, never a workflow edit.

### Timeouts, and why they are what they are

| Budget | Value | Reason |
|---|---|---|
| Per-cell job timeout | 90 min | The tui-migrate cell measured 65 min end to end (image bake + migration + reboot + rollback + commit, driven through the wizard). |
| `e2e-gate` wait window (ci.yml) | 100 min | Must clear the job timeout plus queue time. It was 50 min while the matrix was gated off; leaving it there would have failed every PR the moment the matrix started gating. |

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
catches; **the pinned legacy builder drifting to new-gen is the critical
alert** — it breaks the delegation ladder and blocks the MVP.

Maintaining the baseline: when a drift is real and absorbed (code adapted,
docs updated), update `canary-baseline.tsv` in the same PR that absorbs it.

## Flake policy & known gotchas

- **Retry taxonomy**: GHCR connection-resets and ENOSPC during image pull
  are infra flakes (rerun the cell via narrow dispatch); assertion failures
  and phase errors are never rerun without a diagnosis.
- **`gh run rerun` reuses the run's original merge commit.** If main moved
  since (e.g. a harness fix landed), rerunning tests stale code — merge
  main into the PR branch instead. This has bitten before.
- **Cancelled runs surface as `fail`** in `gh pr checks`; a stuck-queued
  workflow sometimes needs cancel + rerun to re-enter the queue.
- **Disk sizing is part of the test.** ENOSPC inside the guest shows up as
  misleading downstream errors (including probe misfires — see the probe
  asymmetry note in CONTEXT.md). Cells document their disk_size rationale
  inline in e2e-tests.yml.
- **Mount errors on new-gen hosts are expected noise** in phases 4/5 until
  PR #76 lands (podman fallbacks carry the migration) — don't read them as
  the failure signal.
- **The LVM-on-LUKS cell is the disk-tightest one** (60G, BIOS-boot + ESP +
  separate /boot + fixed 4G /var LV before root sees anything) — an ENOSPC
  there is the flake class to suspect first, not a regression.

### Incident: `required-checks` didn't actually gate on `e2e-gate`

PR #73 auto-merged 2026-07-19 while its E2E run showed one cell failing
(the LVM-on-LUKS ENOSPC flake above). Root cause: `required-checks` — the
one job branch protection watches — listed `[validate, cargo-deny,
coverage, actionlint]` in its `needs`, never `e2e-gate`. The two jobs were
siblings, not a chain, since the workflow's original introduction — this
predates every change in this document. `required-checks` passed on the
fast jobs alone while `e2e-gate` independently failed, and GitHub had no
reason to block the merge.

Fixed by adding `e2e-gate` to `required-checks`' `needs`. The rule going
forward: **`required-checks` is the merge gate, so every other gate must
feed into it** — a new CI job that isn't in that `needs` list is invisible
to branch protection no matter how good the job itself is. (The merge
itself needed no revert: the fix under test was validated by the other
three cells, including the critical btrfs one, and the ENOSPC failure was
the known flake class above.)

## Local reproduction

- `just check` — everything CI's validate job runs, now including the
  feature matrix (`test-all-features`).
- `just e2e` / `e2e-lts` / `e2e-luks` / `e2e-lvm` — full cells locally;
  `e2e-status`, `e2e-ssh`, `e2e-tail`, `e2e-scan`, `e2e-reboot-test` for
  surgical iteration on a phase.
- `just drift-canary` — the upstream probe, sans CI.
- Cross-generation store experiments: reproduction commands in
  docs/cfs-cli-generations.md §Reproducing (needs a `mkfs.ext4 -O verity`
  loopback; root filesystems commonly lack the verity feature).

## Gaps / next automation (tracked, not yet built)

1. **Store-level integration test in CI**: the loopback cross-gen check
   (legacy write → new-gen fsck) as a cheap weekly job — no VM, ~2 GB.
2. **Nightly `cargo update --dry-run` + composefs-rs version watch** —
   surfacing new 0.x releases early (NativeStore pins 0.7).
3. **Merge-queue discipline**: once the stacked fleet lands, enable GitHub
   merge queue with `required-checks` as the single required context (it
   already tolerates skipped optional jobs).
