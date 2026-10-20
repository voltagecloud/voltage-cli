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

## Before you request review

Authors and reviewers, people or agents, run the same checks. CI does not run all of them,
so a pull request is not ready until each one is done and reported.

1. Review the diff against its base with the `voltage-pr-review` skill from
   [voltagecloud/engineering-skills](https://github.com/voltagecloud/engineering-skills).
   For Rust changes, also read the `voltage-backend-engineering` skill and its Rust style
   checklist. Fix each applicable finding before you ask for review.
2. Test every new error and early-return path, not only the success path.
3. Run the verification gate in the [development guide](docs/development.md).
4. When the change touches `src/` or `tests/`, run the
   [CRAP report](docs/quality.md#crap-risk-check). It is required even though CI does not
   run it: every measured function must stay below 20. Fix a violation in the pull request
   that introduces it, by adding meaningful tests or simplifying the code, never by
   splitting functions only to lower the score.
5. In the pull request description, give the checks you ran and the CRAP result: the
   highest score and each function at or above 20. Name any check you could not run, and
   why.

A reviewer runs the CRAP report on the pull request's head and reports each violation as a
finding, together with the review from step 1.

## Agent friction log

The CLI must be easy for AI agents to use. When it is not, record why in
`docs/issues/`, which git ignores. Each record is local to your checkout and feeds later
fixes. Record an issue when any of these happens while you use `voltage`:

- A command exits with an error code that you then work around. A failed guess at a flag,
  a server rejection that the CLI could have caught first, and a hint that names no next
  step all count. An exit code that reports a deliberate outcome, such as 6 for a
  described change, counts only if it misled you.
- You use a tool other than `voltage` to do the job, such as `jq`, `python`, or a shell
  loop over its output, or you read the source or README to learn how to use a command.
- You take many steps to learn why you cannot do something. For example, you look
  through every organization before you find that no wallet can pay an invoice.

Write one directory per issue, `docs/issues/<YYYY-MM-DD>-<short-slug>/`, with a
`README.md` that gives:

1. **What happened**: the goal, the commands you ran, and the exact errors and exit
   codes, with the placeholders that the rule below this list requires.
2. **Why it was confusing**: what the CLI said, what an agent needs, and the cheapest
   fix.
3. **Provenance**: the date, the branch and commit (`git rev-parse --short HEAD`), the
   `voltage --version` output, the deployment (local, staging, or production), and the
   session or task that found it.
4. **Status on other branches**: work is often on unmerged branches. Look at them
   (`git branch -a --no-merged origin/master`) and name any branch that fixes or reduces
   the issue.
5. **Reproduction**: a reproducible setup in a `repro.sh` that runs in the Nix dev shell
   (`nix develop --command ./docs/issues/<dir>/repro.sh`). Pin the commit it builds and
   use `scripts/voltage-local` or `scripts/voltage-staging` with placeholders for
   account-specific IDs. When the issue needs live state, such as an empty wallet, give
   the steps that create that state.

Keep each record generic, so anyone can read it and run it. Put no personal or account
data in it: no names, email addresses, organization, wallet, or environment names, real
IDs, invoices, addresses, balances, counts of a person's resources, hostnames, or local
paths. Use placeholders such as `ORG`, `WALLET_ID`, and `INVOICE`. Use the placeholder UUID
(`00000000-0000-4000-8000-000000000000`) for offline runs. Remove secrets, tokens, and API
keys. Before you save a record, search it for UUIDs, `lnbc`/`lntb` invoices, and names.

Before you add an issue, look for an existing directory and add to it. Now and then, and
always after a fix merges, run each `repro.sh` against the current `master`. Delete each
directory whose issue is fixed, and update each directory that has changed.
