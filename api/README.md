# API contracts

`openapi.json` is the unmodified Voltage API snapshot from [the public OpenAPI endpoint](https://voltageapi.com/v1/openapi/docs.json), refreshed on 2026-09-18. It contains 47 operations. `commands.json` explicitly assigns each operation a human-facing command and positional resource ID.

`build.rs` derives HTTP methods, paths, authentication selection, parameters with their closed value sets, and body support from the snapshot, and takes each operation's command words and positional target from `commands.json`. The build fails on a snapshot operation without a mapping, a mapping without an operation, a command that is not lowercase kebab-case words, or a target that is not a placeholder in the operation's path. `src/registry.rs` deserializes the result into closed enums, so an operation ID, auth scheme, method, or target the code does not know fails `cargo test`. The integration suite exercises every registered operation through a mock HTTP server, including every query parameter, complete JSON bodies, empty responses, and credential selection.

`auth-openapi.json` is the auth service's public OpenAPI document, generated from `auth-service` master (`ApiDoc::public_api()`) on 2026-09-18. The deployed service serves it at `https://auth.voltage.cloud/api/v1/openapi/docs.json` once the device-login release ships. The CLI does not generate code from it; `auth::tests::the_cli_uses_only_endpoints_and_fields_in_the_auth_contract` pins the endpoints, request fields, and response fields that device login, refresh, exchange, revocation, and discovery rely on, so a contract change fails `cargo test`.

`scripts/check-coverage.py` verifies both contracts and generates `docs/commands.md`: the API operations from the snapshot and mapping, and the login, credential, and profile commands with the auth-service routes they call (each route must exist in `auth-openapi.json`).

`coinprice-openapi.json` is the price service's public OpenAPI document from `https://coinprice.voltage.cloud/openapi.json`, fetched on 2026-09-18. `voltage price` and `voltage convert` use its one route; `price::tests::the_cli_uses_only_the_route_and_fields_in_the_coinprice_contract` pins the route and the `CurrencyPrice` fields, and `scripts/check-coverage.py` checks the route again when it generates the command reference.

To update the API contract, fetch the endpoint into `openapi.json`, review its diff, update the command registry and any new authentication/pagination behavior, regenerate the command reference with `python3 scripts/check-coverage.py --write`, and run the full Rust checks. Never silently rename existing commands as part of generation.

To update the auth contract, fetch the public document into `auth-openapi.json`, review its diff, and run `cargo test --locked`; adjust `auth.rs` and its contract test together when a field or endpoint changes. The price contract refreshes the same way from `coinprice-openapi.json` and `price.rs`.
