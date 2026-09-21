# Development guide

Status: maintained alongside the code

Related: [README](../README.md), [style guide](quality.md), [API contracts](../api/README.md), and
[command reference](commands.md).

## Build

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use. No
system library is needed: the Linux Secret Service client is pure Rust.

```sh
cargo build --locked
./target/debug/voltage --help
```

Project-local tool installs belong under the gitignored `.work/` directory, for example
`cargo install --root .work/tools ...`, so the global `~/.cargo/bin` stays untouched.

## Verification gate

Run before every push. CI runs the same commands.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo machete --with-metadata
cargo test --locked
python3 scripts/check-coverage.py
```

Integration tests start mock HTTP servers on port 0 and never contact a real service. They
cover every operation in the contract, query encoding, complete JSON bodies, credential
selection, confirmation, secret output, pagination, payment waits, uncertain submissions,
checkout tokens and streams, and the credential lifecycle. Live payment tests must use an
explicitly configured test-network wallet; routine tests never send a live payment.

The manual CRAP risk check is described in [the style guide](quality.md#crap-risk-check).

## Continuous integration and releases

`.github/workflows/ci.yml` runs on pull requests, pushes to `master`, and on demand:

- `lint` (Ubuntu): formatting, Clippy with warnings denied, `cargo machete`, and the
  contract mapping check.
- `test` (macOS arm64 and x86-64, Ubuntu x86-64 and arm64): compiles and runs the whole
  suite, then packages the release archive and smoke-tests the extracted binary.
- `windows`: Clippy and the unit tests, then the package smoke test. The integration suite
  asserts Unix ownership and permissions, so it does not run on Windows.

`.github/workflows/audit.yml` audits `Cargo.lock` against RustSec daily and whenever a
manifest or lock file changes.

Releases are tag driven (`.github/workflows/release.yml`). Push a stable `vX.Y.Z` tag that
matches the `Cargo.toml` version and sits on `master`:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The workflow verifies the tag, reruns the gate on every platform, builds the five archives
(`scripts/package.sh`), writes `SHA256SUMS` and the Homebrew formula
(`scripts/release-manifest.py`), and opens a draft GitHub release. Publish the draft only
after the staging acceptance below is recorded.

## Contracts

The API snapshot, the explicit command mapping, and the auth contract live under `api/`;
[api/README.md](../api/README.md) explains their provenance and how to refresh each one.
`build.rs` generates the operation registry from the snapshot and mapping and fails the
build when they disagree. `docs/commands.md` is generated; regenerate it with
`python3 scripts/check-coverage.py --write` after a contract change and never edit it by hand.

## Scripts

- `scripts/voltage-local` runs the development binary with isolated credentials, loopback
  auth on port 8081, and a disabled API endpoint. `--build` compiles it first.
- `scripts/voltage-staging` runs the development binary against staging. It reads the
  staging endpoints (`VOLTAGE_STAGING_API_URL`, `VOLTAGE_STAGING_AUTH_URL`) and, only for
  API-key runs, the optional `VOLTAGE_API_KEY`, `VOLTAGE_ORGANIZATION_ID`,
  `VOLTAGE_ENVIRONMENT_ID`, and `VOLTAGE_WALLET_ID` from the repository's private `.env`
  (copy `.env.example`, fill it in, `chmod 600 .env`) without evaluating it as shell code,
  keeps CLI state under `.work/staging/cli`, and refuses `--config-dir`, `--api-url`, and
  `--auth-url` so a staging command cannot target another deployment. Browser login and
  profiles need no credential variables. The repository itself names no deployment. Before
  creating a payment, confirm the wallet's reported network is a test network.
- `scripts/package.sh TARGET` builds the release archive for one Rust target and smoke-tests
  the extracted binary.
- `scripts/release-manifest.py VERSION DIR` writes `SHA256SUMS` and `voltage.rb` for the
  archives in `DIR`.
- `scripts/check-coverage.py` verifies that the API snapshot and the command mapping agree,
  that every auth-service route the login, credential, and profile commands use exists in
  the auth contract, and that `docs/commands.md` is current (`--write` regenerates it).
- `scripts/crap-report.sh` measures the CRAP score of every function; see the style guide.

## Running against staging

`scripts/voltage-staging` is a drop-in `voltage` for the staging deployment. Alias it for the
shell session so commands read exactly as they do in the README, then work through a profile
the same way you would against production.

1. Copy `.env.example` to `.env`, set `VOLTAGE_STAGING_API_URL` and
   `VOLTAGE_STAGING_AUTH_URL` (ask the team for the current hosts), and `chmod 600 .env`.
   Leave the credential variables commented out; browser login needs none of them.
2. From the repository root, alias the wrapper and build the development binary:

   ```sh
   alias voltage="$PWD/scripts/voltage-staging"
   voltage --build
   ```

3. Sign in through the browser and discover your organization and environment. These two
   list commands require a browser login; an API key cannot use them. `--credential-store
   file` keeps the login under `.work/staging/cli` with the rest of the staging state.

   ```sh
   voltage login --account me --credential-store file
   voltage organizations list --account me
   voltage environments list --account me --org ORG_ID
   ```

4. Save a profile and use it exactly as the README quick start does for production:

   ```sh
   voltage profiles create staging --account me --org ORG_ID --env ENV_ID
   voltage wallets list --profile staging
   voltage payments receive --profile staging --wallet WALLET_ID \
     --currency btc --kind bolt11 --amount 1000 --unit sats --qr
   ```

   Once the profile exists, fold it into the alias to drop the flag as well:
   `alias voltage="$PWD/scripts/voltage-staging --profile staging"`.

5. When finished, revoke the session and remove every trace of the staging state:

   ```sh
   voltage logout --account me
   rm -rf .work/staging
   unalias voltage
   ```

The wrapper keeps staging state apart from `~/.config/voltage`, passes the endpoints from
`.env` as `--api-url` and `--auth-url`, and refuses those flags and `--config-dir` on the
command line. A profile ignores `VOLTAGE_WALLET_ID` and the other ambient variables, so pass
`--wallet` explicitly. Confirm a wallet's reported network is a test network before creating
a payment. Browser login needs the device-login release deployed on the staging auth service.

## Staging acceptance

Complete these steps against the deployed staging services before publishing a release, and
record the deployed revisions and results in the validation baseline below.

1. Confirm browser login, refresh, and organization exchange against the deployed auth
   service, and that its verification URL points at the deployed approval page.
2. Complete CLI approval with an existing browser session, with a fresh login, and with MFA.
3. Verify denial, expiry, polling slowdown, single redemption, and independent sessions on two
   devices.
4. Verify organization discovery, organization switching, exchanged API access, and
   permission rejection.
5. Verify concurrent local processes, refresh rotation, replay detection, logout, and global
   signout.
6. Check native credential storage on each release platform and explicit file storage on
   headless Linux.
7. Verify ingress rate limits and code/token redaction.
8. Run payment and checkout acceptance with an explicitly selected test-network wallet.
