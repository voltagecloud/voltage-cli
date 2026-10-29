# Voltage CLI development shell (not a release package). `kai shell` enters it.
# The Rust toolchain comes from rust-toolchain.toml, as it does for rustup and CI.
app [kaifile] {
	pf: platform "https://github.com/thebrandonlucas/kai/releases/download/v0.0.9/EenHNXeRqmvf2kHJLLbjroXeLwbQxyH5fbVQVgD3DXg4.tar.zst",
	std: "https://github.com/thebrandonlucas/kai/releases/download/v0.0.9/std-DiShoF1FRWjQe7BmRnSTmqG44WgHUGuMMTMgBoaBqemq.tar.zst",
}

import std.Std

import "rust-toolchain.toml" as toolchain : Str

kaifile = Std.kaifile([
	Name("voltage-cli"),
	Systems(["x86_64-linux", "aarch64-linux"]),
	Overlay("rust", "github:oxalica/rust-overlay"),
	Environment(
		"dev",
		[
			RustToolchain(toolchain),
			Overlays(["rust"]),
			Tools(["python3", "cargo-machete"]),
		],
	),
	Shell("default", [Use("dev")]),
])
