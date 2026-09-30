# Re-base Engine Design

**Status**: accepted design; phase selection landed in `bootc-rebase` for [RFC #30]
**Scope**: target architecture to expand `bootc-migrate` into a re-base engine.
This document is the design counterpart to the RFC. It does not change
current behavior. The plan follows phases M1–M3 from the RFC.

[RFC #30]: https://github.com/tuna-os/bootc-migrate/issues/30

---

## 1. Problem restated

Today the pipeline is a single scenario: OSTree(bootc) → composefs(bootc) with
GRUB → systemd-boot. The code will split phase functions (`phase1_import_objects`
… `phase5_setup_bootloader`), but compiles the scenario into binary code.
The sequence, bootloader policy, and /etc + UID logic live in `migration/mod.rs`
and `migration/deploy.rs` (#123).

The RFC has five axes:

| # | Scenario | New machinery |
|---|----------|---------------|
| A | OSTree → OSTree (same family) | skip composefs phases; keep bootloader |
| B | GRUB → systemd-boot standalone | first-class `migrate-bootloader` |
| C | Cross-base (Fedora → CentOS) | UID/GID remap, /etc conflict policy, SELinux relabel |
| D | Cross-image (Bluefin → tuna-os) | none beyond C + catalog |
| E | DE config carry-over (GNOME ↔ KDE) | pluggable stash/restore hook |

The trap to avoid: adding `--backend`/`--base` flags that switch inside the
existing God-modules. That compounds #123. The generalization **is** the
refactor: extract a planner + phase interface, then express every scenario
(A–E) as a phase selection over one pipeline.

## 2. Core model: `RebasePlan`

A run is a plan, not a branch. The plan is built by a planner and executed by
the pipeline; both live in `bootc-migrate-core`.

```rust
pub struct RebasePlan {
    pub source: ImageDescriptor,   // current deployment, read from ostree/state
    pub target: ImageDescriptor,   // image ref from the catalog / CLI
    pub mode: RebaseMode,          // which scenario family (see §4)
    pub phases: Vec<PhaseSpec>,    // ordered phase selection for this mode
    pub policies: Policies,        // bootloader, uid/gid, /etc conflict (§5)
    pub hooks: Vec<HookSpec>,      // DE-migrate + user hooks (§6)
}

pub struct ImageDescriptor {
    pub base: BaseFamily,          // fedora | centos | ...
    pub backend: Backend,          // ostree | composefs (bootc internals cfs)
    pub bootloader: Bootloader,    // grub | systemd-boot
    pub verity: bool,              // fs-verity sealed
    pub cpu: Option<CpuLevel>,     // x86-64-v2/v3 — for cross-image guards
}
```

The system gets `ImageDescriptor` from `os-release` and BLS inspection for
source, and the registry manifest for target (in `registry.rs`).

## 3. Phase pipeline

Keep the phase split. Give it an interface and context so phases do not use
global state:

```rust
pub trait RebasePhase {
    fn name(&self) -> &'static str;
    fn required_by(&self, mode: &RebaseMode) -> bool;   // hard dependency
    fn optional_for(&self, mode: &RebaseMode) -> bool;  // may be skipped
    fn run(&self, ctx: &mut PhaseContext) -> Result<PhaseReport>;
}

pub struct PhaseContext<'a> {
    pub plan: &'a RebasePlan,
    pub report: &'a PreflightReport,
    pub dry_run: bool,
    pub force: bool,
    // scratch dirs, lock file, progress sink — no module globals
}
```

Phases (current names kept where possible):

| Phase | Today | In the engine |
|-------|-------|---------------|
| preflight | `preflight.rs` | unchanged; gains cross-base checks (UID divergence) |
| import | `phase1_import_objects` | unchanged |
| pull | `phase2_pull_image` | registry streaming stays (§7) |
| seal | `phase3_create_image` | **only when target is composefs** — skipped in A/B |
| deploy | `phase4_stage_deploy` | split: `var`/user carry-over vs. image stage |
| bootloader | `phase5_setup_bootloader` | policy-driven (§5.1); reused by standalone B |
| rollback/commit | `transaction.rs` | unchanged; covers the whole phase set |

Each phase returns a `PhaseReport` (changes, skipped items, delta size).
The CLI and future TUI render the same report.

## 4. Mode matrix

The planner selects phases per mode. `--backend=ostree` (A) is *not* a flag on
the pipeline; it is a mode that omits `seal` and pins `bootloader=keep`.

| Mode | import | pull | seal | deploy | bootloader | /etc merge | UID remap |
|------|:------:|:----:|:----:|:------:|:----------:|:----------:|:---------:|
| A ostree→ostree | ✔ | ✔ | ✘ | ✔ | keep | ✔ | ✘ |
| B bootloader-only | ✘ | ✘ | ✘ | ✘ | target | ✘ | ✘ |
| C cross-base | ✔ | ✔ | ✔/✘ | ✔ | per §5.1 | conflict policy | ✔ |
| D cross-image | ✔ | ✔ | ✔/✘ | ✔ | per §5.1 | conflict policy | ✔ |
| E (hooks) | — | — | — | — | — | — | — |

`migrate-bootloader` (B) reuses `phase5_setup_bootloader` with a plan whose
only phase is bootloader — no new code path.

The executable counterpart is `crates/bootc-rebase/src/routing.rs::plan`.
`bootc-rebase --plan` prints the selected phases and bootloader policy
without host changes. The planner is pure and has tests for all four backend
pairs. Strategy execution remains behind protected paths until trait
extraction is complete.

The composefs→ostree route runs as `Strategy::OstreeInstall` (#260). The
target's `bootc install to-existing-root` runs alongside the composefs root.
`bootc-migrate-core::ostree_install` owns the ESP snapshot, `/etc` merge, and
`/var` copy.

For frontends and tools, `bootc-rebase --plan-json` emits the route as a JSON
object (`from`, `to`, `strategy`, `implemented`, `phases`, and `bootloader`).
It implies `--plan`. No preflight, registry access, or filesystem mutation
occurs. This keeps the plan contract clean without screen scrapes. It makes
reverse routes that lack support explicit before a user tries an apply.

## 5. Decision policies (RFC open questions 1–3)

### 5.1 Bootloader (Q1)

**Default: keep the source bootloader** for A (ostree→ostree) — minimal-change
principle; a bootloader swap is risk with no user-visible benefit. Migrate to
systemd-boot only when the **target mandates it** (composefs targets ship
systemd-boot by default, as today) or the user explicitly asks (standalone B).
This makes B a standalone capability, not an automatic side effect.

### 5.2 UID/GID remap (Q2)

**Auto-remap with a report; refuse only on ambiguous collisions.** A migration
tool that hard-refuses by default is a dead end for its main users. Rules:

- Remap is per-entry (`/etc/passwd`, `/etc/group`, `/etc/shadow`) using the
  target's `base` defaults; the report lists every remapped entry.
- **Hard refuse** (needs `--force`) if two users from the source have the same
  target UID. This prevents data errors.
- The tool rewrites `/var` and `/home` ownership before the bootloader phase.
  This prevents mismatched file ownership.

### 5.3 /etc merge conflict policy (Q3)

Keep the 3-way merge (old-default ∆ current → new-default). It works for
standard cases. When *both* current and target change the same key, write a
`.rpmnew` sidecar file. Add a summary line to the final report so the user
resolves conflicts. Reuse `etc_conflict.rs`, which provides this logic.

## 6. DE-migrate hook contract (Q4)

Design the DE translation as a **typed manifest over stdin + exit-code
contract**, not an env-var handshake:

```
plugin run  <<JSON   # {"action":"stash"|"restore","de":"gnome","user":"alice",...}
              JSON
exit 0  → success; report on stdout (JSON)
exit 3  → "nothing portable" (not an error)
other  → failure; stderr carries the reason
```

- Stash location: `~/.local/share/de-migrate/<from-de>/` (namespaced, not
  deleted — enables round-trip restore per the RFC).
- The engine runs plugins before and after phases as `HookSpec` entries. A
  missing plugin binary triggers a warning, not a failure.
- The GNOME↔KDE translation stays outside the core engine. The hook contract
  provides the extension point.

## 7. Acquisition strategy (unchanged)

Registry stream extraction (`extract_files_via_registry`,
`extract_subtree_via_registry`, `extract_kernel_modules_via_registry`)
remains the acquisition path. See `docs/architecture.md` §1–2 for details
(EROFS zero-fill, ENOSPC with `podman cp`). The planner does not choose a
different method.

## 8. Refactor boundary with #123

This is the key rule: extract the planner and phase interface to resolve #123.
Do not defer this work. For `migration/deploy.rs` and `migration/boot.rs`:

1. M1 splits `deploy` logic into two phase structs with no behavior change.
2. M1 moves bootloader policy to `policies.rs` (§5.1 rules). `phase5_setup_bootloader`
   executes the policy.
3. M2 (cross-base) adds `uid_gid.rs` + extends `etc_conflict.rs` — no growth of
   the God-modules.

Anything that does not fit a phase or a policy belongs in a new module, not in
`mod.rs` glue.

## 9. Test strategy (Q5)

- **Phase × mode unit matrix**: every phase declares `required_by` and
  `optional_for`. Unit tests run each mode and assert selections.
- **Policy unit tests**: bootloader policy, refusal for duplicate UIDs, and
  `.rpmnew` sidecar files run on any host.
- **E2E**: keep the existing composefs leg; add C (stable→LTS) and D
  (stable→tuna-os) legs once M2 lands. The phase-selection unit matrix is the
  fast guard; E2E legs are the slow proof.

## 10. Phasing map (RFC M1–M3)

| RFC | Engine work | Modules touched |
|-----|-------------|-----------------|
| M1 | `RebasePlan` + planner + phase trait; `--backend=ostree` (A); standalone B | `migration/plan.rs`, `migration/pipeline.rs`, `policies.rs` (moves from `deploy.rs`/`boot.rs`) |
| M2 | cross-base (C): UID remap, conflict sidecars, SELinux relabel | `uid_gid.rs`, `etc_conflict.rs` |
| M3 | DE hooks (E): manifest contract + stash/restore | `hooks.rs` + `payload/` plugins |

---

*Drafted by the architect agent as the design counterpart to RFC #30 and
reviewed against `docs/architecture.md` (lessons) and the #111/#123 God-file
findings. Phase selection is now executable; destructive phase extraction is
still deliberately staged behind the protected MVP.*
