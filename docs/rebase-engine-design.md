# Re-base Engine Design

**Status**: DRAFT — architect design for [RFC #30]
**Scope**: target architecture for generalizing `bootc-migrate` from a
composefs-only migrator into a bootc image re-base / migration engine.
This document is the design counterpart to the RFC; it does not change
current behavior. Phasing follows the RFC's M1–M3.

[RFC #30]: https://github.com/tuna-os/bootc-migrate/issues/30

---

## 1. Problem restated

Today the pipeline is a single fixed scenario: OSTree(bootc) → composefs(bootc)
with GRUB → systemd-boot. The phase functions are already split
(`phase1_import_objects` … `phase5_setup_bootloader`, `run_rollback`,
`migrate_bootloader_standalone`), but the *scenario* is compiled in: the
sequence, the bootloader policy, and the /etc + UID handling are hard-wired in
`migration/mod.rs` and `migration/deploy.rs` (the #123 God-modules).

The RFC generalizes along five axes:

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

`ImageDescriptor` is cheap to obtain: `os-release` + BLS inspection for source,
registry manifest for target (already implemented in `registry.rs`).

## 3. Phase pipeline

Keep the existing phase split (they are already the right granularity) but give
it an interface and a context so phases stop reaching into globals:

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

Each phase returns a `PhaseReport` (what changed, what was skipped, size of
delta) — the CLI and the future TUI render the same report.

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
- **Hard refuse** (require `--force`) when two distinct source users collide on
  the target UID — that is a data-integrity ambiguity, not a preference.
- `/var` and `/home` ownership are rewritten in the same pass, before the
  bootloader phase, so the system never boots with mismatched ownership.

### 5.3 /etc merge conflict policy (Q3)

Keep the 3-way merge (old-default ∆ current → new-default) — it is already
correct for the common case. New rule for the cross-base case: when *both*
current and target modified the same key (true conflict), write a
`.rpmnew`-style sidecar next to the file and add one summary line to the
final report the user resolves, rather than silently preferring either side.
Reuse `etc_conflict.rs`; it already implements most of this.

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
- The engine calls plugins pre- and post-phase as `HookSpec`s in the plan; a
  missing plugin binary is a warning, never a failure of the migration.
- The GNOME↔KDE translation itself stays outside the core engine (per RFC out
  of scope), but the contract is the extension point.

## 7. Acquisition strategy (unchanged)

Registry streaming (`extract_files_via_registry`, `extract_subtree_via_registry`,
`extract_kernel_modules_via_registry`) remains the only acquisition path — see
`docs/architecture.md` §1–2 for why (EROFS zero-fill past 4 KB, ENOSPC with
`podman cp`/`skopeo`). The planner does not get to choose a different strategy.

## 8. Refactor boundary with #123

This is the key constraint: **the planner + phase interface extraction is the
fix for #123**, so it must not be done as a separate "cleanup" after the
feature work. Concretely for `migration/deploy.rs` (1,660 LOC) and
`migration/boot.rs` (1,651 LOC):

1. M1 moves `deploy`'s image-stage vs. var/user carry-over into two phase
   structs (same code, new seams) — pure move, no behavior change.
2. M1 moves bootloader policy out of `boot.rs` into `policies.rs` (the §5.1
   rules), keeping `phase5_setup_bootloader` as the executor.
3. M2 (cross-base) adds `uid_gid.rs` + extends `etc_conflict.rs` — no growth of
   the God-modules.

Anything that does not fit a phase or a policy belongs in a new module, not in
`mod.rs` glue.

## 9. Test strategy (Q5)

- **Phase × mode unit matrix**: every phase declares `required_by`/`optional_for`;
  a unit test iterates all modes × phases and asserts the expected selection
  (§4 table is executable). This catches a regression in A/B the moment C/D
  land.
- **Policy unit tests**: bootloader keep-vs-migrate, UID remap collision
  refusal, `.rpmnew` sidecar generation — all pure, runnable on any host.
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

*Drafted by the architect agent as the design counterpart to RFC #30.
Reviewed against `docs/architecture.md` (lessons) and the #111/#123 God-file
findings; deliberately a docs-only change.*
