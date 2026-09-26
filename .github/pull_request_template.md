## Summary
<!-- What does this PR change, and why? Ensure the description accurately reflects the diff. -->

## Validation & Testing
<!-- Detail the exact tests and checks that were executed and their results. -->
- [ ] `just check` (or unit tests + clippy + rustfmt + shellcheck) run and passed
- [ ] E2E scenario / live testing performed (if applicable)
- [ ] All validation caveats discharged (no unverified "deferred to CI" or environment blockers)

## Definition of Done Checklist
- [ ] Claims in the PR title, body, and comments accurately match the diff
- [ ] Non-trivial logic includes strict table-driven unit tests or integration tests
- [ ] If validation was deferred to CI, verified that CI workflows actually ran and reported green on the head commit
- [ ] If live validation is deferred to a future phase, documented in `ROADMAP.md` "Unvalidated paths" with linked tracking issue
- [ ] Commits follow `component: Summary` and carry a valid Signed-off-by trailer (`git commit -s`)
