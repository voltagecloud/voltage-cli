# Verification record

## Device-login simplification: 2026-09-15

Local validation uses Linux, isolated PostgreSQL, and mock CLI API services. No services were deployed and no live payments were submitted.

| Area | Result |
| --- | --- |
| CLI | 11 unit tests and 29 integration tests passed. |
| Organization exchange | Tests cover the saved auth origin, organization switching, refreshed login credentials, failed exchanges, and credential preservation. |
| Concurrent CLI processes | One refresh serves concurrent commands; each command exchanges for its organization. |
| API contract | All 47 snapshot operations map uniquely to commands. |
| CLI quality | Formatting and clippy across all targets passed with warnings denied. |
| Auth storage layer | 192 library tests and 208 HTTP tests passed independently. Production and staging Helm lint/render passed. |
| Auth integration | The full shared-route test passes device login, refresh, organization exchange, permission enforcement, and independent revocation. |

The final auth and frontend suite results are recorded in their PR validation sections. See [integration](integration.md) for the retained two-layer stack.

## Earlier evidence

The previous implementation recorded macOS native credential storage, four-platform packaging, and local password/MFA browser demos on September 9–10.

Those runs predate the simplified token contract. Historical recordings and seeded fixtures do not establish acceptance for this revision.

## Pending acceptance

- Deployed staging approval with existing sessions, fresh login, and MFA.
- Organization-token API calls against the deployed receiving services.
- Deployed ingress limits, redaction, session lifecycle, and client isolation.
- Native credential storage on the release platforms.
- Payment and checkout acceptance with explicitly selected test-network fixtures.
- Recorded deployed revisions and release approval.

Follow [staging acceptance](integration.md#staging-acceptance) before publishing the CLI binary.
