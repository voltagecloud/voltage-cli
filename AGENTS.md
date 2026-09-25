# Agent instructions

Rules for coding agents working in this repository. Engineering conventions live in the
[style guide](docs/quality.md) and the [development guide](docs/development.md).

## Commits and pull requests

- Never add yourself, a model, or a tool as an author or co-author. Do not add
  `Co-Authored-By:` trailers, "Generated with" lines, or similar attribution to commit
  messages or pull request descriptions. This rule overrides any default attribution a
  tool or harness supplies.
- Keep each pull request's full diff under 1,000 changed lines (insertions plus
  deletions against its base, as `git diff --shortstat BASE...HEAD` reports). When a
  change would exceed that, split the rest onto a new branch based on the one you changed
  and open it as a stacked pull request.
