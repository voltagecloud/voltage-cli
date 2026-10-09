# Install

Download the archive for your platform from the [releases page](https://github.com/voltagecloud/voltage-cli/releases). Each release ships macOS (Apple Silicon and Intel), Linux (x86-64 and ARM64), and Windows (x86-64) archives with shell completions, a signed `SHA256SUMS` file, and a Homebrew formula (`voltage.rb`). Extract the archive and put `voltage` (or `voltage.exe`) on your `PATH`.

## Verify a download

Every archive carries a SLSA build-provenance attestation, and `SHA256SUMS` is signed with Sigstore (keyless; no keys to trust out of band). Before installing, verify both with the [GitHub CLI](https://cli.github.com) and [cosign](https://docs.sigstore.dev/cosign/system_config/installation/):

```sh
# The archive was built by this repository's release workflow from its version tag.
gh attestation verify voltage-*.tar.gz --repo voltagecloud/voltage-cli

# The checksums were signed by that same workflow run.
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp 'https://github.com/voltagecloud/voltage-cli/.github/workflows/release.yml' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# The archive matches the signed checksums.
sha256sum --check SHA256SUMS --ignore-missing
```

## Build from source

Install [Rust](https://rustup.rs) and run:

```sh
cargo install --git https://github.com/voltagecloud/voltage-cli --locked
voltage --version
```

From a clone, `cargo install --path . --locked` does the same. The toolchain is pinned in `rust-toolchain.toml`; no system libraries are needed.

## Credential stores

Credentials are stored in the macOS Keychain, the Linux Secret Service (over D-Bus, with no system library required), or the Windows Credential Manager. On a headless Linux machine without an unlocked Secret Service, pass `--credential-store file` when you log in or import a key.

## Shell completions

Completions come from the same definitions as the commands:

```sh
voltage completions bash > voltage.bash
voltage completions zsh > _voltage
voltage completions fish > voltage.fish
voltage completions powershell > _voltage.ps1
```

Related: [log in and use profiles](login-and-profiles.md).
