# Local auth, frontend, and CLI validation

The auth PRs now use a registered-client and stored-consent contract. The earlier Python auth launcher and seeded one-command demo have been deferred to a separate development-tooling proposal. Existing local fixture data is preserved, but the old startup commands are no longer supplied by the auth stack; use a fresh disposable test database for its changed, unmerged migration.

Current review stack:

- Auth [#328](***REMOVED-PRIVATE-REPO***) → [#329](***REMOVED-PRIVATE-REPO***) → [#330](***REMOVED-PRIVATE-REPO***) → [#327](***REMOVED-PRIVATE-REPO***), all ready for review.
- [Frontend #2560](***REMOVED-PRIVATE-REPO***): displays the registered application and stored access, and binds the decision to the request shown.
- [CLI #1](https://github.com/voltagecloud/voltage-cli/pull/1): device login, credentials, discovery and API commands.
- The backend API and checkout must deploy strict OAuth issuer/audience/scope validation before these tokens are released to CLI users.

Run auth's Rust/PostgreSQL tests using its existing database configuration. Run the frontend's mock suite with `node packages/e2e/scripts/test-cli-authorization.mjs`; it exercises consent, session recovery and no-JavaScript approval without deployed credentials. Run the CLI checks described in its README. See [auth validation and rollout](***REMOVED-PRIVATE-REPO***) for the current contract and acceptance requirements.

The frontend helper `node scripts/dev-cli-auth.mjs` and this repository's `./scripts/voltage-local --build` still support a separately configured loopback auth service at port 8081. The wrapper isolates CLI credentials and disables wallet/payment requests by default. It does not start auth, provision accounts or start a wallet backend. Browser login requires the updated frontend, a fresh/auth-compatible database and matching local signing keys. Deployed accounts and tokens cannot be substituted for local fixtures.

The previous opt-in recording harness expects the deferred seeded auth environment. Historical recordings demonstrate the earlier flow, not the new grant contract; do not treat them as current staging acceptance. Repeating that recorded demo requires a separately agreed local setup. The existing `.local-dev` data and pre-feedback Git backup are preserved for that work.
