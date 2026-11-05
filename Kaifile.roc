# Voltage CLI development shell (not a release package). `kai shell` enters it.
# The Rust toolchain comes from rust-toolchain.toml, as it does for rustup and CI,
# plus llvm-tools-preview for `scripts/crap-report.sh`.
app [kaifile] {
	pf: platform "https://github.com/thebrandonlucas/kai/releases/download/v0.0.11/65bk6qdahqxn1mm6LR6Hkyw8dSdoeRZGwjNSuiPfvguP.tar.zst",
	std: "https://github.com/thebrandonlucas/kai/releases/download/v0.0.11/std-6Jw5GxNCFkpnvBbnFmQg4KAVEn5bAfEveSZfDSzTDs42.tar.zst",
}

import std.Std

import "rust-toolchain.toml" as toolchain : Str

# Build in the checkout, not the Nix store. Cargo's incremental build is a no-op
# when sources have not changed.
build_on_enter =
	\\if [ -f Cargo.toml ] && [ -f rust-toolchain.toml ]; then
	\\  if cargo build --locked; then
	\\    export PATH="$PWD/target/debug:$PATH"
	\\  else
	\\    echo "Voltage build failed; target/debug was not added to PATH." >&2
	\\  fi
	\\else
	\\  echo "Enter the development shell from the repository root to build voltage." >&2
	\\fi

kaifile = Std.kaifile([
	Name("voltage-cli"),
	Systems(["x86_64-linux", "aarch64-linux", "aarch64-darwin"]),
	Overlay("rust", "github:oxalica/rust-overlay"),
	Environment(
		"dev",
		[
			RustToolchainWith(toolchain, [Components(["llvm-tools-preview"])]),
			Overlays(["rust"]),
			Tools(["python3", "cargo-machete"]),
		],
	),
	Shell("default", [Use("dev")]),
	Raw("nix", "shell:default", Attrs([("shellHook", Str(build_on_enter))])),
])
