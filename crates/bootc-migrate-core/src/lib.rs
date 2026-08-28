//! In-place migration from OSTree-backed to composefs-backed bootc systems.
//!
//! This library exposes the building blocks used by the
//! `bootc-migrate` binary so other tools (e.g. a universal
//! bootc re-base engine) can compose their own migration pipelines:
//!
//! - [`boot_audit`] — UEFI boot-entry enumeration + dead/generic/duplicate/firmware classification
//! - [`boot_cleanup`] — the destructive half of that audit: a pure planner for
//!   entry removal/branding-rename, and the `efibootmgr` executor it authorizes
//! - [`de_detect`] — desktop-environment detection for a target image (registry-streamed) or the live host
//! - [`de_controller`] — read-only policy for planning cross-desktop config migration
//! - [`de_migrate`] — cross-DE config stash/restore, portable-subset extraction, hook contract
//! - [`mergetc`] — 3-way /etc merge, identity DB union, dangling-symlink pruning
//! - [`etc_conflict`] — cross-base /etc conflict policy applied to a deployment
//!   already staged by `bootc switch` (target defaults win, `.rebase-old` sidecars)
//! - [`rebase_plan`] — the backend-pair routing table: which re-bases are
//!   supported, by what strategy, and in what phase order
//! - [`reflink`] — CoW-aware file copy (FICLONE with fallback)
//! - [`xattr`] — xattr-preserving copy helpers
//! - [`ostree`] — OSTree object scanning and hashing
//! - [`composefs`] — composefs image operations (via `bootc internals cfs`)
//! - [`registry`] — disk-bounded, layer-at-a-time file extraction from OCI images
//! - [`preflight`] — system introspection and migration readiness checks
//! - [`migration`] — the phase 0–5 migration pipeline, bootloader/BLS handling,
//!   kernel command-line construction, and os-release parsing
//! - [`transaction`] — two-phase apply: `commit` / `undo` of a staged migration
//! - [`types`] — shared types such as [`VerityDigest`]

pub mod boot_audit;
pub mod boot_cleanup;
pub mod composefs;
#[cfg(feature = "composefs-native")]
pub mod composefs_native;
pub mod cross_base;
pub mod de_controller;
pub mod de_detect;
pub mod de_migrate;
pub mod etc_conflict;
pub mod mergetc;
pub mod migration;
pub mod motd;
pub mod ostree;
pub mod preflight;
pub mod rebase_controller;
pub mod rebase_plan;
pub mod reflink;
pub mod registry;
pub mod remap;
pub mod scan;
pub mod selinux;
pub mod steam_flatpak;
pub mod transaction;
pub mod types;
pub mod xattr;

pub use types::VerityDigest;
