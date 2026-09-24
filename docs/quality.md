# Voltage CLI engineering style guide

Status: active project convention

Related: [README](../README.md), [development guide](development.md),
[API contracts](../api/README.md), and [command reference](commands.md).

These rules are normative for new and changed code. Existing deviations are
cleanup debt, not precedent. Human and agent reviewers check every pull request
against this guide. A pull request must explain each exception as described in
[Review exceptions](#review-exceptions).

These rules define the required standard, not proof that every path already
meets it. Check the current implementation before copying a pattern. Use the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for shared
library design, subject to these CLI-specific rules.

## Design

- Start with a concrete type. Put behavior in its inherent `impl` blocks.
- Use a closed enum when the CLI knows all implementations.
- Use a closure or function parameter to replace one operation.
- Add a project trait for a current production substitution boundary or a
  shared behavioral contract used by a common algorithm. Testing and possible
  future use are not sufficient. Explain the production reason in the pull
  request.
- Substitute an external system in tests with a hand-written, domain-owned
  `*Substitute`. Keep it under `#[cfg(test)]` unless integration tests require
  an intentional public boundary. Use a mocking framework only when a
  substitute is impractical.
- Use standard and framework traits for Rust interoperability.
- Keep orchestration, authorization, and state transitions explicit. Extract
  repeated mechanics without hiding operation order or failure behavior.
- Pass dependencies through constructors. Do not use mutable global state or a
  service locator.
- Extend an existing command, service, or request path with compatible defaults
  before adding a parallel path.

## Types and APIs

- Expose fields directly on records, commands, wire values, and read views.
  Keep invariant-bearing fields private behind validated operations.
- Do not add trivial getters, setters, or convenience `Deref` implementations.
  A read-only getter is an exception only when a caller needs state that must
  remain private; explain the protected invariant and caller need in the pull
  request. Prefer validated domain operations over setters.
- Borrow inputs when an operation only inspects or temporarily mutates them.
  Take ownership when it retains or consumes the value. Pass small `Copy`
  values by value; do not borrow merely to clone internally.
- Use `as_*` for borrowed projections, `into_*` for consuming conversions, and
  `view()` only for a real read or persistence boundary.
- Use `From` for infallible, lossless conversions and `TryFrom` when validation
  can fail. Reject out-of-range numeric values instead of narrowing them with
  `as`.
- Use enums for domain alternatives and typed payloads for variant data. Do not
  encode alternatives as primitives or mutually exclusive `Option`s. Use
  `bool` only for an independent yes/no fact.
- Give an alternative with its own behavior or invariants one named concrete
  payload type, then let the enum match and delegate shared operations. Keep
  simple statuses and data-only variants direct.
- Flat wire or storage records can use optional fields only when conversion
  immediately validates them into the typed domain form.
- Match enums exhaustively. Convert them to primitives only at external
  boundaries and test stable encodings. Use `Unknown` only when a protocol
  requires safe forward compatibility.
- Keep `Eq`, `PartialEq`, `Ord`, `PartialOrd`, and `Hash` consistent. Prefer
  compatible derives and test relationships implemented by hand.
- Use a newtype for validation, units, ambiguity, or security meaning. Use
  typestate only when it prevents a materially invalid transition.
- Validate wire and persisted values before constructing domain types. Private
  fields and Serde derives do not by themselves preserve constructor
  invariants.
- Use `Instant` for elapsed time and deadlines. Use `SystemTime` only for
  wall-clock values and convert to the protocol's required encoding at the
  boundary.

## Money

- Keep amounts in the API's integer base units: msats for BTC and cents for
  USD. Never use floating point for money, rates, fees, or limits.
- Use raw integers only at wire edges. Put the unit in names such as
  `amount_msats`, or convert immediately to `payment::Amount` or a typed price
  value.
- Check bounds before multiplying units or adding a rounding offset. Use
  checked, currency-aware arithmetic and widen intermediate rate and fee math
  to `u128`.
- State and test each conversion's rounding direction. Test every precision and
  rounding boundary, the largest supported value, overflow, and underflow.
- Sum exact base-unit values before converting them for display.

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
  rehydration. Fail closed on unknown states, invalid widths, impossible
  relationships, missing evidence, and missing required configuration.
- Bound untrusted bytes, counts, depths, queues, requests, responses, and
  concurrent work before allocating or spawning. Define and test behavior at
  each limit.
- Aggregate input errors deterministically. Return one error for one
  operational failure.
- Use typed errors at every CLI-owned fallible boundary. Give variants
  actionable classes, typed context, and sources.
- Do not use `String`, maps, or `anyhow` as domain errors. Convert to
  framework-required text only in adapters.
- Classify a failure as retryable or permanent where it is handled. Retry only
  documented reads or polls; never turn a permanent rejection into a loop.
- Branch on error variants, HTTP statuses, or typed provider codes, never on
  display text.
- Map errors exhaustively to the documented exit codes (see the README
  "Exit codes" table). Return a safe message plus typed detail, never raw
  internal text.
- Restrict type erasure to top-level aggregation. Do not panic on command-line
  input, API responses, saved state, or long-lived work. Use `expect` only for
  build-time, startup, or static invariants and name the invariant.
- Bound network, asynchronous lock, polling, streaming, and cleanup waits with
  `--timeout` or a named constant. Keep synchronous lock sections short.

## Persistence and HTTP

- Keep `config.toml`, credential files, the request journal under `requests/`,
  and the lock file private: owner-only files in an owner-only directory.
  Create them with `private_dir` and `new_private`, check them with
  `check_private`, and replace them with `atomic_write`, all in `config`.
- Default to the native credential store. Use file storage only on an explicit
  `--credential-store file`. There is no plaintext fallback.
- Build every HTTP client with a request and connection timeout, redirects
  disabled, and automatic retries disabled. Never retry a mutation. Reconcile
  an uncertain submission by reading its original ID; a missing projection
  stays uncertain.
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

- Prefer single ownership, message passing, or immutable snapshots over shared
  mutable state. When mutation is necessary, lock only the state that protects
  the invariant and keep the critical section short.
- Release synchronous lock guards before `.await` or network I/O. Review async
  lock hold time and cancellation behavior.
- Put cancellation first: Ctrl-C must win, exit 130, and never resend a
  submitted mutation. Check cancellation safety before racing an operation
  against Ctrl-C or a timeout; preserve recovery information for an effect
  whose outcome remains unknown.
- Retain and await spawned task handles. Dropping a `JoinHandle` detaches the
  task, and cancelling its waiter does not stop it.
- Move blocking I/O and sustained CPU work to `spawn_blocking`, with bounded
  admission and a completion or cancellation mechanism. Running blocking work
  cannot be aborted.
- Bound every wait, poll, and pagination loop by `--timeout`. Exit 5 when the
  deadline passes and include the original resource ID of a pending payment.
- Poll only with server-provided retry hints or documented intervals. Do not
  use sleeps to establish order or readiness. In tests, use paused Tokio time,
  channels, or explicit state transitions.
- Give secrets dedicated types and narrow lifetimes. Accept them through the
  environment, stdin, a private file, or native credential storage, never a
  command-line argument.
- Keep owned plaintext in `Secret`, `Zeroizing`, or another type that zeroizes
  on drop. Expose it only for the required operation, avoid unnecessary clones
  and formatting, and drop temporary buffers promptly. Document unavoidable
  plaintext copies and library-owned buffers in the pull request.
- Do not derive `Debug` or `Serialize` for plaintext secret holders. A custom
  `Debug` implementation must redact. Never log or return credentials, tokens,
  or complete configuration values.
- Use safe Rust by default. Use `unsafe` only at a required external boundary.
  Keep each block small and put a `SAFETY:` explanation immediately before it.
- Explain validity, ownership, lifetime, and release. Give owned raw resources
  a tested safe wrapper with `Drop`.

## Tests and documentation

- Name tests after observable behavior.
- Assert real values, including payloads, limits, filters, and stable
  encodings. Size fixtures so a missing bound or filter fails the test. Do not
  remove or weaken existing tests.
- Keep rule tests beside code. Put route, wire, process, and platform behavior
  in integration tests.
- Prefer concrete components and project-owned substitutes over mock traits.
- Test success, rejection, bounds, transitions, stable encodings, restarts,
  and trust-boundary isolation when applicable. Add replay or "already exists"
  coverage to every idempotency, conflict, or uncertain-outcome fix.
- Use fixtures for exact bytes and temporary directories with port `0`.
- Use mock HTTP servers; never depend on developer state or real services. Use
  paused Tokio time for timers and timeouts for real waits.
- Coordinate concurrent tests with channels or notifications. Cancel and await
  spawned tasks. Test cancellation, saturation, and restart recovery when task
  or queue ownership changes.
- Test amount arithmetic, parsers, and encodings at boundary values. Use
  property tests or fuzzing when combinations exceed practical fixtures.
- Do not add line-hit tests only for coverage.
- Document invariants, lifecycle, trust boundaries, and surprising decisions.
  Do not restate syntax. Shared APIs document applicable errors, panics,
  cancellation behavior, side effects, and safety requirements.
- Use `#[expect(lint, reason = "...")]` for a narrow lint exception.
- Link each TODO to its owning ticket. Do not leave unowned placeholders.
- Keep one maintained explanation per topic. Link to it instead of repeating
  architecture, procedures, or acceptance lists across documents.

## Dependencies and checks

- Declare dependencies once in `Cargo.toml`. Enable every required feature
  explicitly where it is used and add dependencies only with their first
  production, test, or build use.
- Review maintenance, licenses, security advisories, enabled features, and
  transitive additions before adding or upgrading a dependency. Keep exact
  direct versions and `Cargo.lock` pinned.
- Use the toolchain in `rust-toolchain.toml`. Update its pin and any workflow or
  packaging references together.
- Record `cargo machete` false positives in
  `[package.metadata.cargo-machete] ignored`.
- Keep throwaway scripts and incidental build output out of the repository.
  Commit required generated contract inputs and documentation.
- Title pull requests with Conventional Commits, such as
  `fix(payments): preserve an uncertain submission`. Use `cli` or the affected
  capability as the scope.

Run the canonical gate:

```console
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo machete --with-metadata
cargo test --locked
python3 scripts/check-coverage.py
```

GitHub Actions runs the same gate on every pull request and push to `master`,
the package smoke test on each release platform, and a daily RustSec audit of
`Cargo.lock`. Do not claim another documentation, audit, or security gate
until the repository configures it.

For documentation-only changes, verify claims, command spellings, relative
links, and examples. Run changed Rust examples and doctests for their owning
module even though CI does not run a separate doctest gate.

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
`target/crap/`. CI does not run it. Add meaningful tests or simplify risky
code. Never game the score with line-hit tests or unnecessary function splits.

## Performance

- Measure before changing a hot path. Record the representative workload,
  release-build baseline, and result in the pull request.
- Check latency, allocations, memory bounds, request count, and contention when
  a change can affect them. Preserve correctness and boundary checks during
  optimization.
- Add a repeatable benchmark when it protects against a material regression.

## Changes across repositories

The API, auth, and price contracts are owned by their respective services.
Check each changed contract in its owning repository, update the snapshots and
tests described in [API contracts](../api/README.md), link dependent pull
requests, and state rollout order. Do not assume repositories deploy together.

## Review exceptions

A pull request must explain each project trait or mocking framework, trivial
getter or setter, convenience `Deref` implementation, unsafe block, lint
exception, dependency, or public item. State the production need and why an
existing simpler pattern cannot satisfy it. For an accessor, also identify the
invariant that requires private state and the caller that needs access.

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
