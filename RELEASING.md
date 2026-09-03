# Releasing

The release contract this project had been missing (#171). Read it before
cutting a tag; the checklist is the contract, not ceremony.

## What ships

`release.yml` packages **`bootc-migrate` only** — the OSTree → ComposeFS
migrator, covered end to end by the E2E matrix including rollback and commit.

`bootc-rebase` is **experimental and deliberately unreleased.** Its
capabilities are implemented, and several are unvalidated on real hardware:
`ROADMAP.md`'s "Unvalidated paths" table enumerates them, with a tracking
issue each. Shipping it under the same stability signal as the proven
migrator would tell users something untrue about paths that rewrite boot
configuration. Build it from source if you want to try it.

If that changes, change it here first, then in `release.yml`.

## Versioning

Numbers must never go backwards for anyone, whichever record they read.

That is not hypothetical here. Until v0.5.0 the git tags (`v0.1.1`, `v0.1.2`,
`v0.2.0`) and `CHANGELOG.md` (`v0.1.0`, `v0.2.0`, `v0.3.0`, `v0.4.0`) were two
independent, contradicting histories: three documented versions were never
tagged, their changelog links 404'd, and the one version in both disagreed on
its date by three months. `v0.5.0` was chosen to sit above the highest tag
*and* the highest documented section, so no reader of either sees a
regression.

Keep them in step from now on:

- `Cargo.toml`'s workspace `version` is the source of truth.
- Every `CHANGELOG.md` section must correspond to a tag that exists. If work
  lands but is not released, it belongs under `[Unreleased]` — not under a
  version heading with a date.
- The tag is `v<version>`, matching Cargo exactly.

## Before tagging

1. `just check` — clippy, rustfmt, unit tests, shellcheck.
2. The **full E2E matrix green on the commit being tagged.** Not a previous
   commit, and not "green last week": these binaries change boot state, and
   the matrix is the only coverage the live paths have. `release.yml` itself
   does not check this — it builds and publishes on any `v*` tag push,
   trusting this step. Verify it with a command instead of memory:
   `./scripts/verify-release-ready.sh <sha-or-ref>` (defaults to `HEAD`)
   checks the GitHub API for a green `CI` and `E2E Migration Tests` run on
   the exact commit and fails if either is missing or red.
3. `cargo deny check advisories bans sources licenses` clean. Run
   `cargo update` first — a stale registry index silently hides yanked
   crates, which is exactly how a yanked `chacha20` sat in the lockfile
   undetected.
4. `Cargo.toml`, `CHANGELOG.md`, and the tag you are about to push all agree.
5. `ROADMAP.md`'s "Unvalidated paths" table is current — the release notes are
   generated from it, and it is how users learn which paths are unproven.

## Cutting it

```bash
git tag -a v0.5.0 -m 'Release 0.5.0' && git push origin v0.5.0
```

The tag push triggers `release.yml`, which builds per-target on native
runners and attaches every binary plus SHA-256 checksums in one
`gh release create`. That single-shot upload is required: this account
enforces immutable releases, so assets must all be present at creation and
cannot be added afterwards. A failed release job means deleting the tag and
starting over, which is the reason for the checklist above.
