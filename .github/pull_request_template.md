<!-- What changed for users, and why. Name the base PR if this is stacked. -->

## Try it on staging

<!-- See AGENTS.md "Try it on staging". Commands a reviewer pastes after the setup. -->

Setup: [Try a pull request on staging](docs/development.md#try-a-pull-request-on-staging)

```sh
voltage ...
```

Expect: ...

## Checks

- [ ] User-visible change: a guide or reference under `docs/` and `CHANGELOG.md` (**Unreleased**) are updated, or the PR has the `no-changelog` label.
- [ ] The verification gate in `docs/development.md` passes.
- [ ] The diff was reviewed against its base with the `voltage-pr-review` skill, and every new error and early-return path has a test.
- [ ] CRAP report, when `src/` or `tests/` changed: the highest score and each function at or above 20.
- [ ] Checks not run, and why:
- [ ] **Try it on staging** lists every changed `--help` and docs example, with what to expect.
