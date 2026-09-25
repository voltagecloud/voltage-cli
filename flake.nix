{
  description = "Voltage CLI development shell (not a release package)";

  inputs = {
    # 26.05 still supports Intel macOS (dropped on current nixos-unstable).
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in {
          default = pkgs.mkShell {
            packages = [
              rust
              pkgs.python3
              pkgs.cargo-machete
            ];
          };
        });
    };
}
