# Releasing

The release contract this project had been missing (#171). Releases are cut
automatically by `release.yml` — there is no tag to push — so read this before
bumping the version, because that bump *is* the release. The checklist is the
contract, not ceremony.

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

- `Cargo.toml`'s workspace `version` is the source of truth — literally, now:
  it is the value `release.yml` reads to decide whether to release.
- Every `CHANGELOG.md` section must correspond to a tag that exists. If work
  lands but is not released, it belongs under `[Unreleased]` — not under a
  version heading with a date.
- The tag is `v<version>`, matching Cargo exactly. Nothing constructs it by
  hand, so the two cannot drift.

## Cutting it

**Releases are automatic. There is no tag to push.**

The workspace `version` in `Cargo.toml` *is* the release decision, and
merging it to `main` is what cuts the release:

```diff
 [workspace.package]
-version = "0.5.0"
+version = "0.6.0"
```

On every push to `main`, `release.yml`'s `resolve` job reads that version and
asks whether `v<version>` is already tagged. If it is, the run stops there and
costs seconds. If it is not, the same run builds both targets, creates the
GitHub Release — which creates the tag — and pushes the container image.

So the release checklist below is a **pull-request** checklist. By the time a
version bump merges, the release has happened.

### Why it is not "tag on main, let the tag trigger the build"

That design is the obvious one and it does not work: **a tag pushed with
`GITHUB_TOKEN` does not start a workflow run.** It produces a tag with no
binaries and no image, which is precisely how `v0.5.0` came to be documented
in `CHANGELOG.md` and never released (#240). Everything therefore happens
inside the single run that made the decision.

The release job attaches every binary plus SHA-256 checksums in one
`gh release create`. That single-shot upload is required: this account
enforces immutable releases, so assets must all be present at creation and
cannot be added afterwards. `gh release create` creates the tag itself and
runs only after both builds succeed, so a failed build leaves no orphan tag
to clean up — re-running the job is the whole recovery.

Pushing a `v*` tag by hand still works and is the escape hatch for releasing
a commit that is not the head of `main`.

## Before merging a version bump

1. `just check` — clippy, rustfmt, unit tests, shellcheck.
2. The **full E2E matrix green on the commit being merged.** Not a previous
   commit, and not "green last week": these binaries change boot state, and
   the matrix is the only coverage the live paths have.
3. `cargo deny check advisories bans sources licenses` clean. Run
   `cargo update` first — a stale registry index silently hides yanked
   crates, which is exactly how a yanked `chacha20` sat in the lockfile
   undetected.
4. `Cargo.toml` and `CHANGELOG.md` agree: the version being merged has a
   `CHANGELOG.md` section, and `[Unreleased]` is empty of anything that
   section does not describe. Whatever is on `main` at merge time is what
   ships under that number.
5. `ROADMAP.md`'s "Unvalidated paths" table is current — the release notes are
   generated from it, and it is how users learn which paths are unproven.

A pull request that does not bump the version needs none of this: merging it
publishes nothing.
