# API contract

`openapi.json` is the unmodified Voltage API snapshot retrieved on 2026-09-09 from [the public OpenAPI endpoint](https://voltageapi.com/v1/openapi/docs.json). It contains 47 operations. `commands.json` explicitly assigns each operation a human-facing command and positional resource ID.

`build.rs` derives HTTP methods, paths, authentication selection, query metadata, and body support from the snapshot. It rejects missing or stale registry entries. The integration suite exercises every registered operation through a mock HTTP server, including every query parameter, complete JSON bodies, empty responses, and credential selection.

To update the contract, fetch the endpoint into `openapi.json`, review its diff, update the command registry and any new authentication/pagination behavior, regenerate the command reference with `python3 scripts/check-coverage.py --write`, and run the full Rust checks. Never silently rename existing commands as part of generation.
