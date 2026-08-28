//! Transition routing: which (source backend → target backend) re-bases are
//! supported, and by what strategy.
//!
//! This is the capability table from issue #30 / #45. Rows are added as
//! engine support lands; `route()` is the single source of truth every
//! frontend consults before touching the system.
//!
//! Lives in the core library rather than in `bootc-rebase` so the executable
//! capability contract has exactly one definition (#160). A frontend that
//! owned its own copy of this table could disagree with the engine about
//! which re-bases exist — the table is a safety surface, not presentation.

use std::fmt;

/// A bootc root-storage backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Classic OSTree deployment (hardlink checkout of an ostree commit).
    Ostree,
    /// ComposeFS-sealed EROFS deployment.
    Composefs,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Ostree => write!(f, "ostree"),
            Backend::Composefs => write!(f, "composefs"),
        }
    }
}

/// How a supported transition is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The proven OSTree → ComposeFS pipeline from bootc-migrate-core
    /// (phases 0–5 with /etc merge, /var carry-over, bootloader switch).
    CoreMigration,
    /// Swap the composefs image in place — no backend conversion needed.
    /// Planned; not yet implemented (issue #30, scenario A analog).
    ImageSwap,
    /// Deploy the target as a plain OSTree deployment, skipping the
    /// composefs phases (issue #30, scenario A). Implemented for
    /// ostree→ostree; composefs→ostree remains planned.
    OstreeDeploy,
}

/// A phase selected by the re-base planner. Keeping this list independent of
/// the implementation functions lets `--plan` be truthful without running a
/// destructive phase, and gives the future pipeline a stable seam for phase
/// reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preflight,
    Import,
    Pull,
    Seal,
    Deploy,
    Bootloader,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Preflight => "preflight",
            Self::Import => "import",
            Self::Pull => "pull",
            Self::Seal => "seal",
            Self::Deploy => "deploy",
            Self::Bootloader => "bootloader",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootloaderPolicy {
    KeepSource,
    Target,
}

/// An executable description of a transition. This is intentionally pure:
/// creating a plan cannot inspect or modify the live system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlan {
    pub route: Route,
    pub phases: Vec<Phase>,
    pub bootloader: BootloaderPolicy,
}

impl RebasePlan {
    pub fn phase_names(&self) -> String {
        self.phases
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

/// A supported (or planned) transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub from: Backend,
    pub to: Backend,
    pub strategy: Strategy,
    /// Whether the engine can execute this today.
    pub implemented: bool,
}

/// The transition table. Ordered; first match wins.
const ROUTES: &[Route] = &[
    Route {
        from: Backend::Ostree,
        to: Backend::Composefs,
        strategy: Strategy::CoreMigration,
        implemented: true,
    },
    Route {
        from: Backend::Composefs,
        to: Backend::Composefs,
        strategy: Strategy::ImageSwap,
        implemented: true,
    },
    Route {
        from: Backend::Ostree,
        to: Backend::Ostree,
        strategy: Strategy::OstreeDeploy,
        implemented: true,
    },
    Route {
        from: Backend::Composefs,
        to: Backend::Ostree,
        strategy: Strategy::OstreeDeploy,
        implemented: false,
    },
];

/// Look up the route for a backend pair. `None` means the transition is not
/// even planned.
pub fn route(from: Backend, to: Backend) -> Option<Route> {
    ROUTES
        .iter()
        .copied()
        .find(|r| r.from == from && r.to == to)
}

/// Select phases from the backend pair. The phase order is the contract in
/// docs/rebase-engine-design.md §4; execution still belongs to the existing
/// strategy implementations until the pipeline extraction lands.
pub fn plan(from: Backend, to: Backend) -> Option<RebasePlan> {
    let route = route(from, to)?;
    let (phases, bootloader) = match (from, to) {
        (Backend::Ostree, Backend::Ostree) => (
            vec![Phase::Preflight, Phase::Pull, Phase::Deploy],
            BootloaderPolicy::KeepSource,
        ),
        (Backend::Ostree, Backend::Composefs) => (
            vec![
                Phase::Preflight,
                Phase::Import,
                Phase::Pull,
                Phase::Seal,
                Phase::Deploy,
                Phase::Bootloader,
            ],
            BootloaderPolicy::Target,
        ),
        (Backend::Composefs, Backend::Composefs) => (
            vec![Phase::Preflight, Phase::Pull, Phase::Deploy],
            BootloaderPolicy::KeepSource,
        ),
        (Backend::Composefs, Backend::Ostree) => (
            vec![
                Phase::Preflight,
                Phase::Pull,
                Phase::Deploy,
                Phase::Bootloader,
            ],
            BootloaderPolicy::Target,
        ),
    };
    Some(RebasePlan {
        route,
        phases,
        bootloader,
    })
}

/// Standalone bootloader migration is a one-phase plan. The live operation
/// remains gated behind issue #65's ESP/NVRAM implementation.
pub fn bootloader_plan() -> RebasePlan {
    RebasePlan {
        route: Route {
            from: Backend::Ostree,
            to: Backend::Composefs,
            strategy: Strategy::CoreMigration,
            implemented: false,
        },
        phases: vec![Phase::Bootloader],
        bootloader: BootloaderPolicy::Target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ostree_to_composefs_is_implemented() {
        let r = route(Backend::Ostree, Backend::Composefs).unwrap();
        assert!(r.implemented);
        assert_eq!(r.strategy, Strategy::CoreMigration);
    }

    #[test]
    fn every_backend_pair_has_a_planned_route() {
        for from in [Backend::Ostree, Backend::Composefs] {
            for to in [Backend::Ostree, Backend::Composefs] {
                assert!(route(from, to).is_some(), "no route for {from} -> {to}");
            }
        }
    }

    #[test]
    fn composefs_to_composefs_is_implemented() {
        let r = route(Backend::Composefs, Backend::Composefs).unwrap();
        assert!(r.implemented);
        assert_eq!(r.strategy, Strategy::ImageSwap);
    }

    #[test]
    fn ostree_to_ostree_is_implemented() {
        let r = route(Backend::Ostree, Backend::Ostree).unwrap();
        assert!(r.implemented);
        assert_eq!(r.strategy, Strategy::OstreeDeploy);
    }

    #[test]
    fn unimplemented_routes_are_marked() {
        assert!(
            !route(Backend::Composefs, Backend::Ostree)
                .unwrap()
                .implemented
        );
    }

    #[test]
    fn planner_skips_composefs_phases_for_ostree_rebase() {
        let p = plan(Backend::Ostree, Backend::Ostree).unwrap();
        assert_eq!(p.phase_names(), "preflight -> pull -> deploy");
        assert_eq!(p.bootloader, BootloaderPolicy::KeepSource);
        assert!(!p.phases.contains(&Phase::Seal));
    }

    #[test]
    fn planner_selects_full_conversion_pipeline() {
        let p = plan(Backend::Ostree, Backend::Composefs).unwrap();
        assert_eq!(
            p.phase_names(),
            "preflight -> import -> pull -> seal -> deploy -> bootloader"
        );
        assert_eq!(p.bootloader, BootloaderPolicy::Target);
    }

    #[test]
    fn planner_covers_every_backend_pair() {
        for from in [Backend::Ostree, Backend::Composefs] {
            for to in [Backend::Ostree, Backend::Composefs] {
                let p = plan(from, to).unwrap();
                assert_eq!(p.phases.first(), Some(&Phase::Preflight));
                assert!(matches!(
                    p.phases.last(),
                    Some(&Phase::Deploy) | Some(&Phase::Bootloader)
                ));
            }
        }
    }

    #[test]
    fn standalone_bootloader_plan_does_not_select_rootfs_phases() {
        let p = bootloader_plan();
        assert_eq!(p.phases, vec![Phase::Bootloader]);
        assert_eq!(p.bootloader, BootloaderPolicy::Target);
    }
}
