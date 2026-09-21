# Security policy

Report vulnerabilities in `voltage` privately to <security@voltage.cloud>. Do not open a public issue or pull request for a security problem.

Include the CLI version (`voltage --version`), the platform, the command that reproduces the problem with credentials and IDs replaced by placeholders, and the observed and expected behavior. Never include a real API key, login token, checkout token, or invoice.

## Verifying releases

Release archives carry [SLSA build provenance](https://slsa.dev) attestations and the `SHA256SUMS` file is signed with [Sigstore](https://www.sigstore.dev) keyless signing; both are bound to the release workflow and version tag in this repository. Verification commands are in the [README](README.md#verify-a-download). If a download fails verification, do not run it and report it to <security@voltage.cloud>.

Only the latest release receives fixes. The machine-readable contact record follows [RFC 9116](https://www.rfc-editor.org/rfc/rfc9116) and lives at [`.well-known/security.txt`](.well-known/security.txt):

```text
Contact: mailto:security@voltage.cloud
Expires: 2027-09-18T00:00:00Z
Preferred-Languages: en
```
