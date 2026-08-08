//! Destructive UEFI boot-entry cleanup (issue #31) — the counterpart to
//! [`crate::boot_audit`]'s read-only classification.
//!
//! The split here is deliberate and is the safety design, not an
//! organizational preference:
//!
//! - [`plan`] is **pure**. Given an audit, a few NVRAM facts, and a user
//!   selection it produces a list of operations or a typed refusal. Every
//!   safety rule this module has — firmware entries are unselectable, the
//!   rollback entry and the currently-booted entry can never be deleted, a
//!   plan may not remove the last bootable entry, an ESP that resolves so
//!   badly that *every* entry looks dead is treated as a wrong ESP rather
//!   than a graveyard — lives there, where it is table-testable without a
//!   UEFI system.
//! - [`live`] is the **executor**: it takes a plan the planner already
//!   approved and turns it into `efibootmgr` calls, after writing a full
//!   NVRAM snapshot to disk. It contains no policy.
//!
//! Nothing in this module runs without an explicit opt-in from the caller;
//! `bootc-rebase boot-entries` previews and exits unless `--apply` is
//! passed. See that subcommand for the user-facing contract.
//!
//! **Validation status**: the planner is unit-tested; the executor's
//! `efibootmgr` interaction cannot be proven by this project's
//! build/clippy/test loop and has no E2E cell. It needs real UEFI
//! hardware or a corral VM (see AGENTS.md) before it should be trusted.

pub mod plan;
