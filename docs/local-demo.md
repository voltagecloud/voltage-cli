# Local auth, frontend, and CLI demo

The three draft PRs work together before merging or deploying:

- [CLI #1](https://github.com/voltagecloud/voltage-cli/pull/1), branch `codex/voltage-cli`.
- [auth-service #327](***REMOVED-PRIVATE-REPO***), branch `codex/cli-device-login`.
- [frontend-turbo #2560](***REMOVED-PRIVATE-REPO***), branch `codex/cli-device-login`.

Keep these PRs in draft until the demo and review preparation are complete. The local stack proves browser password/MFA login, device approval and denial, CLI credentials, account/environment discovery, profiles, refresh rotation, and session-specific logout. It uses the real auth service, its real PostgreSQL migrations and JWT signing, the real frontend, and the development CLI binary.

## Checkouts and prerequisites

This machine has:

| Component | Checkout |
| --- | --- |
| Auth | `~/github/voltagecloud/auth-service` |
| Frontend | `~/github/voltagecloud/frontend-turbo-cli-demo` |
| CLI | `~/github/voltagecloud/voltage-cli` |

The frontend demo is a separate Git worktree. The original `frontend-turbo` checkout and its uncommitted changes remain untouched. You can edit the demo worktree normally; Vite reloads the frontend. Stop and rerun auth after Rust edits, and rebuild the CLI after its edits.

Prerequisites: Rust/Cargo, PostgreSQL 17 binaries, OpenSSL with Ed25519 support, Python 3.9+, Node 22+, and Yarn 4. Docker is unnecessary. On macOS, PostgreSQL binaries are detected in Homebrew; elsewhere set `PG_BIN` to the directory containing `initdb`, `pg_ctl`, and `psql`. The local scripts also recognize the workspace-local Rust installation on this machine if Cargo is absent from `PATH`.

Frontend dependencies are already installed here. For another checkout:

```sh
cd ~/github/voltagecloud/frontend-turbo-cli-demo
YARN_ENABLE_SCRIPTS=false yarn install --immutable
yarn workspace @repo/e2e exec playwright install chromium
```

## Start the three components

Terminal 1 — auth and its dedicated PostgreSQL cluster:

```sh
cd ~/github/voltagecloud/auth-service
python3 scripts/dev.py run
```

Wait for `Auth ready`. First startup builds Rust, generates local signing/MFA keys, applies migrations, seeds two confirmed native-auth users, and enables real TOTP MFA for the second user. Auth listens on `127.0.0.1:8081`; its PostgreSQL cluster listens on `127.0.0.1:55432`. Private state persists under `auth-service/.local-dev/`, which Git ignores. It does not load your regular local auth configuration or need Cognito credentials. Ctrl-C stops the service and the database started by this command.

Terminal 2 — frontend:

```sh
cd ~/github/voltagecloud/frontend-turbo-cli-demo
node scripts/dev-cli-auth.mjs
```

Open [the local frontend](http://localhost:3210). The runner uses HTTP on loopback and proxies browser auth requests through the same origin; server auth calls go directly to port 8081. Use `localhost` consistently so cookies and form origins match. Ctrl-C stops Vite.

Terminal 3 — build and use the CLI:

```sh
cd ~/github/voltagecloud/voltage-cli
./scripts/voltage-local --build
alias voltage="$PWD/scripts/voltage-local"
voltage login --credential-store file
```

The alias applies only to that shell. This wrapper fixes the local auth URL and uses a separate config directory at `voltage-cli/.work/local-demo/cli`. It clears ambient `VOLTAGE_*` variables. Explicit `--credential-store file` keeps this demo's credentials in private files; omit it to exercise the normal macOS Keychain. Add `--no-browser` if you prefer to open the printed URL yourself.

## Record the demo

Start with a private/incognito browser window for a fresh-login demonstration. If a previous CLI login exists, run `voltage logout` before starting another one.

1. Run `voltage login --credential-store file`. Show the verification URL and code in the terminal.
2. Open that URL. Sign in as `developer@example.test`, password `Testing123!`. This is a public development fixture password, valid only in the seeded local database.
3. Show the signed-in account, matching code, and permissions disclosure. Click **Authorize CLI**.
4. Show the terminal's successful login, then run:

```sh
voltage auth status
voltage organizations list
voltage environments list --org 11111111-1111-4111-8111-111111111111
voltage profiles create local \
  --org 11111111-1111-4111-8111-111111111111 \
  --env 22222222-2222-4222-8222-222222222222
voltage profiles get local
voltage environments list --profile local
voltage logout
```

For MFA, use another private browser window and sign in as `mfa@example.test` with the same fixture password. Generate its current code in the auth checkout:

```sh
python3 scripts/dev.py mfa-code
```

The code changes every 30 seconds. Its random secret is kept privately in `.local-dev/mfa-secret`; it can also be enrolled in an authenticator if desired. For denial, start another CLI login and click **Deny access**; the CLI exits with code 3.

## Repeatable verification and recordings

With both servers running:

```sh
cd ~/github/voltagecloud/auth-service
python3 scripts/dev.py smoke

cd ~/github/voltagecloud/voltage-cli
python3 scripts/test-local-demo.py
```

The auth smoke test exercises native password/MFA, pending approval, single-use redemption, discovery, rotation, revocation, and continued browser refresh. The browser suite launches the real CLI against real auth and performs fresh password login, MFA login, and denial. It also tests private credential permissions, profile discovery, automatic refresh, and CLI logout without logging out the browser.

Browser videos are saved as `.work/local-demo/password.webm`, `mfa.webm`, and `deny.webm`; corresponding `*-cli.txt` files record the verified CLI steps. These recordings show the browser; use screen recording with browser and Terminal side by side for a narrated demo. Test credentials are temporary and removed after each test. The suite records visible UI without HAR or token-bearing traces. `--headed` shows the automated browser; `--frontend PATH --auth PATH` selects other checkouts of these PRs.

## Limits and troubleshooting

- This is an auth/discovery demo. Wallet/payment/checkout backends are not started. The local CLI wrapper and frontend runner point those APIs at an unused loopback port. Local signing keys are deliberately different from deployed keys; local JWTs do not authenticate against staging or production APIs.
- Native local accounts bypass the legacy Cognito migration path. This does not demonstrate legacy account migration, email delivery, or deployed ingress behavior. Local analytics are disabled and email is routed to an unused loopback endpoint.
- Auth logs: `auth-service/.local-dev/auth.log`; PostgreSQL logs: `.local-dev/postgres.log`. These directories and CLI credentials are private. Never include token files or signing keys in recordings or commits.
- A port conflict fails startup instead of replacing another service. Auth supports `--port`, `--pg-port`, and `--web-url`; frontend supports `LOCAL_AUTH_URL` and `LOCAL_WEB_PORT`. The CLI wrapper and automated demo intentionally use the documented default ports.
- If a process was forcibly killed, restart `scripts/dev.py run`; it can reuse its private cluster. To reset, stop this checkout's service/cluster first, then remove only its `.local-dev` directory and the CLI's `.work/local-demo/cli` directory. A reset invalidates all old local sessions.
- The deployed staging flow and test-network payment acceptance remain required before releasing the CLI. All three PRs remain drafts; no deployment or release is part of this setup.
