# Verification record

Local verification date: 2026-09-09. Host: macOS Apple Silicon. Rust: 1.98.1. PostgreSQL: isolated local 17.11 database. Frontend: Node 26.7.0 / Yarn 4.4.1. All API/payment fixtures use local mock services; no live funds were moved.

## Passed locally

| Area | Evidence |
| --- | --- |
| API coverage | All 47 snapshot operations map uniquely to commands; HTTP integration exercises paths, authentication schemes, all query parameters, complete bodies, exact int64 values, and empty 202 responses. |
| CLI | 11 unit tests and 21 integration tests. Includes profile precedence, scope mismatch, missing IDs, multiple accounts, confirmation, secret destinations, redaction, cursor/offset behavior, OAuth polling/denial/expiry, concurrent process refresh, logout failures, payment recovery/waiting, and stream interruption. |
| Rust quality | CLI formatting and clippy with warnings denied; auth-service formatting and clippy across all targets/features with warnings denied. |
| Auth-service | 173 library tests plus 199 HTTP integration tests. Ten new device-flow tests use actual PostgreSQL migrations and cover atomic redemption, rotating refresh, replay, expiry, independent/global revocation, existing signing claims, rate limits, and HTTP/OpenAPI behavior. |
| Frontend checks | `yarn run check`: seven tasks passed; Svelte checks report zero errors and zero warnings. Three CLI authorization helper tests passed. |
| Browser flow | Five Chromium tests passed against the isolated mock auth service: login return code, explicit approval, denial/expiry, cross-origin rejection, and native forms with JavaScript disabled. |
| UI review | Desktop and mobile approval/result captures inspected. Initial approval form accessibility scan: zero axe violations. Route design review passed after two specific fixes. |
| Credential storage | Native macOS Keychain write/read/status/logout smoke test with a temporary fake key passed; the entry was removed. Tests reject insecure file credentials and native-store failure without plaintext fallback. |
| Packaging | Apple Silicon release archive built, extracted, and smoke-tested (`--version`, payment help, completions). Other required platforms are configured in the native CI matrix. |
| Service delivery | Both exported patches apply to their recorded base revisions and reproduce all changed files byte-for-byte. See `integration/manifest.json`. |

## Not yet verified or released

- The auth-service and frontend changes have not been pushed, merged, or deployed. Their deployed revisions are **not recorded** because no deployment has occurred.
- Browser approval, issuance, refresh, and revocation have not been exercised together against deployed staging services. Existing-login and fresh-login/MFA acceptance still require staging account access.
- No live payment or checkout integration has run. These require explicitly supplied organization, environment, and test-network wallet identifiers.
- Intel macOS and both Linux archive jobs have not run here. Linux Secret Service behavior and packaged binaries must pass those native jobs before release. Windows packaging is not part of this release.
- The GitHub release workflow and generated four-platform Homebrew formula have not been published. The workflow requires staging verification and creates a draft release for review.

Browser login is a release gate. Local tests establish the source behavior; they do not satisfy the deployed end-to-end requirement. Follow the ordered rollout in [integration.md](integration.md), then replace the pending items with deployed revisions and recorded evidence.
