# Local auth, frontend, and CLI validation

Auth storage [#328](***REMOVED-PRIVATE-REPO***) has landed. [Device login #330](***REMOVED-PRIVATE-REPO***) builds on it and includes its behavioral tests.

[Frontend #2560](***REMOVED-PRIVATE-REPO***) displays the allowlisted application and binds approval to the displayed request. [CLI #1](https://github.com/voltagecloud/voltage-cli/pull/1) exchanges its independent login token before organization API calls. Existing organization-token validation covers those calls.

## Automated checks

Run auth's Rust/PostgreSQL tests against a disposable database. Run the frontend's isolated browser suite:

```sh
yarn workspace @repo/e2e exec playwright install chromium
node packages/e2e/scripts/test-cli-authorization.mjs
```

The dedicated mock runner uses temporary loopback services and fake credentials. It covers consent, session recovery, and approval without JavaScript.

Run CLI formatting, clippy, tests, and operation coverage as documented in [integration](integration.md). See the [device login client contract](***REMOVED-PRIVATE-REPO***) for the request sequence and token behavior.

## Separately configured local services

The frontend helper `node scripts/dev-cli-auth.mjs` and CLI wrapper `./scripts/voltage-local --build` support loopback auth on port 8081.

The CLI wrapper isolates credentials and disables wallet/payment requests by default. It does not start auth, provision accounts, or start an API backend.

Configure matching local signing keys, test accounts, and the frontend verification URL. Use a disposable database with the current migrations applied.

The previous recording harness expects a deferred seeded auth environment. Historical recordings cover an earlier implementation and do not establish acceptance for this revision.
