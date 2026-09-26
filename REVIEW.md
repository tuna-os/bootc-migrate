# Code Review Guidelines

These guidelines mirror those used across the bootc-dev organization (bootc,
composefs-rs). They capture the expectations that have emerged from real review
feedback and apply to both authoring and reviewing changes here.

## Definition of Done (DoD)

A change or milestone is **done** only when both its implementation and its
validation have shipped and are observed passing. Writing code without running
tests or verifying behavior does not satisfy the Definition of Done.

### Core DoD rules

1. **Validation caveats in PR descriptions are blockers, not notes.**
   If an author or tool notes that a test, build, lint, or check could not be run
   (e.g. "could not run tests because the toolchain/linker is missing" or
   "filesystem is full"), the PR is **not ready to merge**. A validation caveat
   is a blocking checklist item that must be discharged by executing the
   validation and recording the passing result before merge.

2. **"Deferred to CI" requires CI to have run and reported green on the head commit.**
   Deferring local validation to CI is only acceptable if CI actually triggers,
   executes the required test suite, and passes on the PR head commit. Silence,
   skipped runs, or missing workflow triggers are not passes. A PR cannot merge
   on unverified deferrals.

3. **Claims in the PR title, body, and comments are part of the change.**
   Reviewers must verify that every assertion in the PR description and code
   comments matches what the diff actually implements. A PR whose description
   claims a security property, bug fix, or validation outcome that the code does
   not implement (e.g. #207 claiming fail-closed HTTPS while leaving fallback
   candidate lists untouched) must be rejected or corrected before merge.

4. **Milestone and tracking issues require full validation to close.**
   An issue is not completed merely because the pure or unit-testable "skeleton"
   landed. If live/system validation (E2E cells, live NVRAM mutation, real firmware
   testing) was part of the issue's scope and has not executed, the issue must
   remain open, or the unvalidated scope must be explicitly transferred to a
   linked validation tracking issue and recorded in [ROADMAP.md](ROADMAP.md)'s
   "Unvalidated paths" table.

5. **Strict, meaningful test assertions.**
   Tests must strictly assert expected state transitions, output values, sidecar
   creation, and error refusals. Tests that merely check that code "didn't crash"
   or that pass vacuously due to early returns violate the DoD. Counter-example:
   #218 shipped a unit test alongside a fix; the test failed immediately against
   the fix (`EspEvidenceImplausible`), demonstrating the discipline working by
   catching the regression before merge.

## Testing

Tests are expected for all non-trivial changes — unit and, where it makes sense,
end-to-end. Never leave validation unexecuted or defer validation with an
undischarged caveat in the PR description.

### Choosing the right test type

Unit tests are appropriate for parsing logic, data transformations, and
self-contained functions. Use the end-to-end suite (`tests/run-e2e.sh`, driven
by `just e2e*`) for anything that involves real disks, mounts, or booting a VM.

Default to table-driven tests rather than a separate `#[test]` per case.
LLMs in particular tend to generate the latter, which gets verbose fast —
context windows matter to both humans and LLMs reading the code later.

### Separating parsing from I/O

Structure code for testability: have a parser accept a `&str` (or `&[u8]`), and
a separate function that reads from disk and calls the parser. This keeps unit
tests free of filesystem dependencies. `os_release.rs` and `kernel_options.rs`
are the model to follow.

### Test assertions

Make assertions strict and specific. Don't merely check that code "didn't
crash" — verify that outputs match expected values.

## Code quality

### Parsing structured data

Never parse structured formats (JSON, INI, etc.) with text tools like `grep` or
`sed`. Use `serde_json` / the `tini` INI parser already in the dependency tree.

### Shell scripts

Avoid shell scripts longer than ~50 lines where a higher-level structure (a
`just` recipe, or Rust glue) would be clearer. `tests/run-e2e.sh` is the one
large exception; keep new logic out of it where practical.

### Constants and magic values

Extract magic numbers and repeated strings into named constants with a comment
explaining any non-obvious choice (buffer sizes, size thresholds, retry counts).

### Don't swallow errors

Avoid `if let Ok(v) = ...` in Rust or `... 2>/dev/null || true` in shell by
default. Most errors should propagate; if one is deliberately ignored, log it
(at least at debug level) and say why. Handle edge cases explicitly — missing
data, malformed input, offline systems — with error messages that give clear
context for diagnosis.

### Code organization

Separate I/O, parsing, and business logic into different functions. Duplicating
a little code twice can be fine; three copies asks for deduplication.

## Rust-specific guidance

Prefer `rustix` over `libc`. `unsafe` is denied at the crate level
(`[lints.rust] unsafe_code = "deny"`); any reintroduction must be very carefully
justified and documented at the call site.

New dependencies should be justified — prefer well-maintained, widely-used crates
and keep `cargo deny` (`deny.toml`) happy. When adding a command or output
format, design for machine-readable output (JSON) early.

## Commits and pull requests

### Commit organization

Break changes into logical, atomic commits a reviewer can follow. Keep
preparatory refactoring separate from behavioral changes.

### Commit messages

Use a `component: Summary` subject in the imperative mood (e.g.
`xattr: share copy helper between mergetc and file copy`). The body should start
with at least a sentence on **why** the change is being made — even for something
apparently trivial. Don't restate what the diff already shows or add redundant
`Changes:`/`Files changed:` sections. Briefly note non-obvious consequences or
discarded alternatives where useful. `Closes:` tags go at the end.

### Follow-up changes

Squash fixups (CI fixes, review-comment applications, auto-generated
"Update <file>" commits) into the commit they belong to. A commit either stands
alone with its own rationale or it should be squashed.

### Before merge

Self-review your diff first against the Definition of Done. Ensure that:
- Every claim in the PR title and description accurately reflects the code diff.
- All local tests or CI workflows have been observed passing on the head commit.
- No undischarged validation caveats remain in the PR description.
- Do not add `Signed-off-by` automatically — that requires explicit human action
  after review. If the change was AI-assisted, include an `Assisted-by:` trailer
  (see [AGENTS.md](AGENTS.md)).

## Architecture and design

When implementing a workaround, document where the proper fix belongs and link
the relevant upstream issue. Prefer pushing fixes upstream when the root cause is
in a dependency (ostree, bootc, composefs). When rewriting functionality, verify
the new code path handles every case the old one did.
