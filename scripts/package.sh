#!/usr/bin/env bash
set -euo pipefail
# Run on the native target runner, after tests.
target=${1:?Rust target is required}
version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
cargo build --release --locked --target "$target"
name="voltage-v${version}-${target}"
directory="target/package/$name"
mkdir -p "$directory/completions" dist
cp "target/$target/release/voltage" "$directory/voltage"
cp README.md "$directory/README.md"
cp -R docs "$directory/docs"
"$directory/voltage" completions bash > "$directory/completions/voltage.bash"
"$directory/voltage" completions zsh > "$directory/completions/_voltage"
"$directory/voltage" completions fish > "$directory/completions/voltage.fish"
tar -czf "dist/$name.tar.gz" -C target/package "$name"
# Smoke-test the actual extracted archive, with no network/credentials.
smoke=$(mktemp -d)
trap 'rm -rf "$smoke"' EXIT
tar -xzf "dist/$name.tar.gz" -C "$smoke"
"$smoke/$name/voltage" --version
"$smoke/$name/voltage" payments send --help >/dev/null
"$smoke/$name/voltage" completions zsh >/dev/null
