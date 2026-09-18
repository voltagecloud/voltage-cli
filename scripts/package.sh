#!/usr/bin/env bash
set -euo pipefail
# Run on the native target runner, after tests.
target=${1:?Rust target is required}
python=$(command -v python3 || command -v python)
version=$("$python" -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])' < <(cargo metadata --no-deps --format-version 1))
cargo build --release --locked --target "$target"
name="voltage-v${version}-${target}"
directory="target/package/$name"
binary=voltage
archive="dist/$name.tar.gz"
if [[ $target == *windows* ]]; then
  binary=voltage.exe
  archive="dist/$name.zip"
fi
rm -rf "$directory"
mkdir -p "$directory/completions" dist
cp "target/$target/release/$binary" "$directory/$binary"
cp README.md "$directory/README.md"
cp -R docs "$directory/docs"
"$directory/$binary" completions bash > "$directory/completions/voltage.bash"
"$directory/$binary" completions zsh > "$directory/completions/_voltage"
"$directory/$binary" completions fish > "$directory/completions/voltage.fish"
"$directory/$binary" completions powershell > "$directory/completions/_voltage.ps1"
if [[ $archive == *.zip ]]; then
  rm -f "$archive"
  (cd target/package && "$python" -m zipfile -c "../../$archive" "$name")
else
  tar -czf "$archive" -C target/package "$name"
fi
# Smoke-test the actual extracted archive, with no network/credentials.
smoke=$(mktemp -d)
trap 'rm -rf "$smoke"' EXIT
if [[ $archive == *.zip ]]; then
  "$python" -m zipfile -e "$archive" "$smoke"
else
  tar -xzf "$archive" -C "$smoke"
fi
"$smoke/$name/$binary" --version
"$smoke/$name/$binary" payments send --help >/dev/null
"$smoke/$name/$binary" completions zsh >/dev/null
