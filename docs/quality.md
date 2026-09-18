# Voltage CLI engineering style guide

Status: active project convention

Related: [README](../README.md), [development guide](development.md),
[API contracts](../api/README.md), and [command reference](commands.md).

These rules are normative for new code. Existing deviations are cleanup debt,
not precedent. Reviews must approve and document exceptions.

## Design

- Start with a concrete type. Put behavior in its inherent `impl` blocks.
- Use a closed enum when the CLI knows all implementations.
- Use a closure or function parameter to replace one operation.
- Add a project trait for a current production substitution boundary or a
  shared behavioral contract used by a common algorithm. Testing and possible
  future use are not sufficient.
- Use standard and framework traits for Rust interoperability.
- Keep orchestration, authorization, and state transitions explicit. Extract
  repeated mechanics without hiding operation order or failure behavior.
- Pass dependencies through constructors. Do not use mutable global state or a
  service locator.

## Types and APIs

- Expose fields directly on records, commands, wire values, and read views.
  Keep invariant-bearing fields private behind validated operations.
- Do not add trivial getters, setters, or convenience `Deref` implementations.
- Use `as_*` for borrowed projections, `into_*` for consuming conversions, and
  `view()` only for a real read or persistence boundary.
- Use enums for domain alternatives and typed payloads for variant data. Do not
  encode alternatives as primitives or mutually exclusive `Option`s. Use
  `bool` only for an independent yes/no fact.
- Flat wire or storage records can use optional fields only when conversion
  immediately validates them into the typed domain form.
- Match enums exhaustively. Convert them to primitives only at external
  boundaries and test stable encodings. Use `Unknown` only when a protocol
  requires safe forward compatibility.
- Use a newtype for validation, units, ambiguity, or security meaning. Use
  typestate only when it prevents a materially invalid transition.

## Modules and imports

- Organize code by the operation that it performs, one module per mechanism.
  `registry` holds the operation contract that `build.rs` generates from
  `api/openapi.json` and `api/commands.json`. `cli` builds the command tree
  and parses arguments into typed values; it is the only module that reads
  `ArgMatches`. `input` constructs and validates request paths, queries, and
  bodies. `api` executes HTTP requests and owns waiting, pagination, and
  streaming.
- Keep device login, token refresh and exchange, and credential selection in
  `auth`. Keep settings, credential storage, and private files in `config`.
  Keep payment values and payment read views in `payment`. Keep secret text
  in `secret`. Keep result envelopes, redaction, and rendering in `output`.
  Keep exit-code categories and typed error detail in `error`.
- Name each module for the operation that it performs. Do not give two modules
  the same capability name. For example, keep private-file mechanics in
  `config` and redaction in `output`.
- Keep items private by default. Export only the required caller boundary and
  re-export the intentional module surface.
- Do not expose an implementation only for a test. Keep `main.rs` small and
  process composition in `startup`.
- Import project items at module scope and use their short names at call sites.
  Import `crate::config::read_secret`, then call `read_secret()`.
- Never call a project item through a `crate::...` path inside a function.
  Resolve collisions with a clear module-scope `as` alias.

## Validation and errors

- Parse and validate external input before domain logic. Reject unknown fields
  when ignoring them could change behavior.
- Validate saved configuration, credentials, and journal records during
  rehydration. Fail closed on unknown states, invalid widths, or impossible
  relationships.
- Bound untrusted bytes, counts, depths, queues, requests, and responses.
- Aggregate input errors deterministically. Return one error for one
  operational failure.
- Use typed errors at every CLI-owned fallible boundary. Give variants
  actionable classes, typed context, and sources.
- Do not use `String`, maps, or `anyhow` as domain errors. Convert to
  framework-required text only in adapters.
- Map errors exhaustively to the documented exit codes (see the README
  "Exit codes" table). Return a safe message plus typed detail, never raw
  internal text.
- Restrict type erasure to top-level aggregation. Do not panic on external
  input.

## Persistence and HTTP

- Keep `config.toml`, credential files, the request journal under `requests/`,
  and the lock file private: owner-only files in an owner-only directory.
  Create them with `private_dir` and `new_private`, check them with
  `check_private`, and replace them with `atomic_write`, all in `config`.
- Default to the native credential store. Use file storage only on an explicit
  `--credential-store file`. There is no plaintext fallback.
- Build the HTTP client with redirects and automatic retries disabled. Never
  retry a mutation. Reconcile an uncertain submission by reading its original
  ID; a missing projection stays uncertain.
- Journal an ID-bearing mutation before submission with its ID, operation,
  scope, timestamp, and request hash, never its body. Reject a reused ID with
  a different payload.
- Keep checkout credentials separate from account credentials. Never fall back
  from one to the other. Prove the separation.
- Generate commands, routes, and docs from the one operation registry
  (`build.rs`, `scripts/check-coverage.py`).
- Use direct wire fields and explicit Serde names. Use `deny_unknown_fields`
  when omission could remove a precondition, but not on forward-compatible
  responses.
- Record the login's auth origin with its account. Refresh, exchange, and
  revoke that credential only at the recorded origin.

## Lifecycle and security

- Put cancellation first: Ctrl-C must win, exit 130, and never resend a
  submitted mutation. Use `spawn_blocking` for blocking work.
- Bound every wait, poll, and pagination loop by `--timeout`. Exit 5 when the
  deadline passes and include the original resource ID of a pending payment.
- Poll only with server-provided retry hints or documented intervals. Do not
  coordinate with sleeps in tests. Use paused Tokio time, channels, or explicit
  state transitions.
- Give secrets dedicated types and narrow lifetimes. Never expose plaintext.
  Avoid `Clone`, `Serialize`, and ordinary `Debug`; bound reads and zeroize
  owned buffers.
- Use safe Rust by default. Use `unsafe` only at a required external boundary.
  Keep each block small and put a `SAFETY:` explanation immediately before it.
- Explain validity, ownership, lifetime, and release. Give owned raw resources
  a tested safe wrapper with `Drop`.

## Tests and documentation

- Name tests after observable behavior.
- Keep rule tests beside code. Put route, wire, process, and platform behavior
  in integration tests.
- Prefer concrete components and project-owned substitutes over mock traits.
- Test success, rejection, bounds, transitions, stable encodings, restarts,
  and trust-boundary isolation when applicable.
- Use fixtures for exact bytes and temporary directories with port `0`.
- Use mock HTTP servers; never depend on developer state or real services. Use
  paused Tokio time for timers and timeouts for real waits.
- Cancel and await spawned tasks. Do not add line-hit tests only for coverage.
- Document invariants, lifecycle, trust boundaries, and surprising decisions.
  Do not restate syntax.
- Use `#[expect(lint, reason = "...")]` for a narrow lint exception.
- Do not leave unowned placeholders. Label target and implemented behavior.
- Keep one maintained explanation per topic. Link to it instead of repeating
  architecture, procedures, or acceptance lists across documents.
- Record batch-by-batch test history in commits and CI. Keep only the current
  validation baseline and remaining acceptance in
  [the development guide](development.md#validation-baseline).

## Dependencies and checks

- Declare dependencies once in `Cargo.toml`. Enable features at the consumer
  and add dependencies only with their first production use.
- Pin dependencies in `Cargo.lock`.

Run the canonical gate:

```console
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo machete --with-metadata
cargo test --locked
python3 scripts/check-coverage.py
```

GitHub Actions runs the same gate on every pull request and push to `main`,
the package smoke test on each release platform, and a daily RustSec audit of
`Cargo.lock`. Do not claim another documentation, audit, or security gate
until the repository configures it.

### CRAP risk check

Change Risk Anti-Patterns (CRAP) is manual. Every measured function must remain
below 20; a score of 20 fails.

The script pins `cargo-llvm-cov` 0.8.5 and `knots` 1.16.0 and refuses other
versions. Install them once; `--root .work/tools` keeps `knots` out of
`~/.cargo/bin`:

```bash
cargo install cargo-llvm-cov --version 0.8.5 --locked
cargo install knots --version 1.16.0 --locked --root .work/tools
KNOTS_BIN=.work/tools/bin/knots scripts/crap-report.sh
```

The script builds an instrumented binary and runs the whole test suite, which
takes several minutes, and writes `crap.md` and `crap.json` below
`target/crap/`. CI does not run it. Add meaningful tests or simplify risky code. Never game the score
with line-hit tests or unnecessary function splits.

## Review exceptions

A pull request must explain each project trait, trivial getter, unsafe block,
lint exception, dependency, or public item. State the production need and why
an existing simpler pattern cannot satisfy it.

### Approved exceptions

- `check_owner` in `config.rs` calls `libc::geteuid` in an `unsafe` block
  because std has no safe effective-UID accessor; the block is one call with a
  `SAFETY:` comment.
- `config`, `registry`, and `secret` are public modules. The integration
  suite seeds saved credentials through the real persistence boundary and
  walks the operation contract to exercise every route; no other item is
  exported for tests.
- `Settings.dir` and `Settings.config` are public record fields so the
  integration suite can construct a configuration directory directly.
