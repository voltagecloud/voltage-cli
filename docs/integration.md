# Browser login integration and release gate

Browser login is a coordinated change across three repositories. A CLI binary alone cannot enable it. The device flow follows [RFC 8628](https://www.rfc-editor.org/rfc/rfc8628); revocation invalidates refresh capability, with existing access JWT lifetime unchanged.

## Source changes

The CLI implementation is in this repository. Auth-service and frontend changes are developed in isolated checkouts on `codex/cli-device-login` and exported into `integration/` as patches with their base commits. Apply each patch to its matching repository with `git apply --check` first, or use the accompanying isolated checkout. The existing frontend checkout and its uncommitted work are untouched.

Auth-service adds:

- A PostgreSQL migration for hashed device/user codes, expiry, polling state, approving browser session, and single-use consumption; independent CLI session IDs; refresh-token history; shared rate-limit buckets.
- Four public-client endpoints under `/api/v1/oauth`: `device_authorization`, `device/decision`, `token`, and `revoke`, documented in its OpenAPI output.
- A 600-second approval window, a five-second initial polling interval, and `authorization_pending`, `slow_down`, `access_denied`, and `expired_token` responses.
- Transactional device redemption and session creation. Approval requires a valid account JWT and an active session from completed browser authentication; logging that approving session out before redemption invalidates approval.
- Access JWTs signed using the existing keys, issuer, and current organization permissions, with `sid`, `jti`, and `client_id` identifiers. CLI refresh tokens are opaque random secrets, not browser JWT refresh tokens.
- Transactional refresh rotation with an unchanged absolute session expiry. Reuse of a consumed refresh token revokes that CLI session. The legacy refresh query explicitly excludes CLI sessions. Global signout already deletes all sessions and therefore covers CLI sessions too.
- No-store responses and query omission from OAuth request logs. Configure ingress logs to omit bodies, authorization headers, and OAuth query strings as well.

Frontend adds `/cli/authorize`, reuses login/MFA through the existing browser session, preserves the verification-code return destination, and forwards decisions server-side. The page shows email, code, and permission scope; it requires approval or denial and rejects cross-origin form submissions. Analytics are suppressed on the approval route and login/MFA return paths. These responses use no-store and `Referrer-Policy: same-origin` (codes are not sent to other origins, and native form Origin headers remain usable). Account tokens are never placed in approval URLs or page data.

## Configuration

Auth-service defaults `VOLTAGE_CLI_VERIFICATION_URI` to `https://app.voltage.cloud/cli/authorize`. Set it to the staging frontend's approval URL in staging. HTTPS is required except for loopback development.

The Helm chart exposes `cliOAuth.verificationUri` and `cliOAuth.trustProxyHeaders`. Its staging values use the existing `https://nextgen.staging.voltage.cloud/cli/authorize` frontend. The existing auth ingress is `https://auth.staging.voltage.cloud/api/v1`; confirm these remain the intended staging hosts before rollout.

Rate limits use the peer socket IP by default. Set `OAUTH_TRUST_PROXY_HEADERS=true` only when the trusted ingress **overwrites** `X-Real-IP`; otherwise leave it disabled. Never trust a client-supplied forwarded IP. Limits are shared in PostgreSQL: 30 device starts/minute/IP, 240 token requests/minute/IP, 10 decisions/minute/account, and 60 revocations/minute/IP. Expired authorization and rate-limit rows are pruned on requests.

The frontend uses its existing auth URL configuration (`PUBLIC_AUTH_URL` / server `PROXY_TARGET_AUTH_URL`). No embedded CLI client secret or new user password flow is needed. Session expiry uses the existing refresh lifetime configuration.

## Verification before deployment

Auth-service: apply migrations to an isolated PostgreSQL database and run formatting, clippy, existing authentication tests, and the new `device_oauth` tests. The SQLx offline query cache includes the changed legacy refresh predicate. Its existing CI runs all tests with PostgreSQL.

Frontend: run `yarn run check`, the CLI helper Jest tests, and `node packages/e2e/scripts/test-cli-authorization.mjs`. The runner starts two loopback services on temporary ports, writes fake browser cookies to a private temporary file, runs Chromium, and cleans up. It explicitly clears real account environment variables. Install the browser with `yarn playwright install chromium` first. The accompanying GitHub workflow runs this mock suite on frontend changes.

The browser tests use the existing Page Object Model and auth fixtures and deliberately skip without `E2E_CLI_MOCK_ORIGIN`, preventing accidental real authorization submissions. Do not interpret mock-browser tests as proof of deployed login or MFA. The local runner is used because Docker was unavailable in this development workspace.

CLI: run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and `python3 scripts/check-coverage.py`. Native CI jobs package and smoke-test macOS ARM64/Intel and Linux ARM64/x86-64. Linux binaries target glibc environments and depend on D-Bus/Secret Service for native storage; explicit file storage is available without a desktop session.

## Staging rollout

1. Deploy the additive auth migration and service first. Confirm all four OAuth endpoints are present in the deployed auth OpenAPI contract and existing browser login/refresh still work.
2. Deploy the frontend approval route using frontend-turbo's documented release procedure. Set the auth service's verification URI to that staging route.
3. Run the CLI with a fresh private config directory and `--auth-url` pointing at staging. Complete approval with an existing login, a fresh login, and an MFA-enabled account. Check that the terminal and browser codes match.
4. Verify denial, expiry, early-poll slowdown, duplicate redemption, independent sessions on two devices, concurrent local processes, refresh rotation, session-specific logout, and global signout. Confirm an old refresh token fails and its session cannot continue refreshing.
5. Verify discovery and explicit profile/account precedence. Perform payment integration only using an explicitly selected **test-network** wallet. Test empty accepted responses, invoice readiness versus completion, timeout reconciliation, and checkout origin/stream credentials.
6. Check secret-store behavior on both supported operating systems, including an unavailable native store and explicit file storage. Confirm codes/tokens are absent from service, ingress, frontend analytics, and CLI diagnostics.
7. Record the deployed service/frontend revisions and evidence in `docs/verification.md`. Only after these checks pass, tag the CLI and dispatch the release workflow with staging verification affirmed. It creates a draft GitHub release with the four archives, checksums, and a concrete Homebrew formula for review before publishing.

Deployment access, staging account/MFA credentials, and explicitly configured test wallets are external prerequisites. This implementation does not provision or modify them, and local tests cannot substitute for the deployed acceptance gate.
