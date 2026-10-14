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
