## Summary

<!-- What does this change and why? Link the issue it resolves (Closes #123). -->

## Checklist

- [ ] Commits follow `type(scope): summary`
- [ ] `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` pass
- [ ] `cargo test --workspace --all-features` passes (plus `scripts/test-postgres.sh` / `scripts/test-redis.sh` if storage, state or the bus changed)
- [ ] Console changes: `npm run typecheck`, `npm run lint`, `npm test` and `npm run check:api` pass
- [ ] Tests cover the change
- [ ] User-visible behaviour is documented and noted in `CHANGELOG.md` under `## [Unreleased]`
- [ ] Docs changes: new pages are in `docs/SUMMARY.md` and `scripts/docs-check.sh` passes
- [ ] `mb-ext` changes: semver impact stated (additive or breaking)

## Notes for reviewers

<!-- Anything that needs a closer look, trade-offs, follow-ups. -->
