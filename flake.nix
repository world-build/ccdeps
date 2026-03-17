{
  description = "High-performance C preprocessor dependency scanner";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type);
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ rustToolchain ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        cppscan = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

        cppscan-clippy = craneLib.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });

        cppscan-fmt = craneLib.cargoFmt {
          inherit src;
        };

      in {
        packages = {
          default = cppscan;
          inherit cppscan;
        };

        checks = {
          inherit cppscan cppscan-clippy cppscan-fmt;
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ cppscan ];

          packages = with pkgs; [
            rust-analyzer
            cargo-watch
            cargo-flamegraph
            cargo-expand

            # Reference compilers for conformance testing
            gcc
            clang
          ];

          shellHook = ''
            echo "cppscan development shell"
            echo "========================="
            echo ""
            echo "  cargo build --release   Build the scanner"
            echo "  cargo test              Run tests"
            echo "  cargo bench             Run benchmarks"
            echo "  cargo run -p cppscan-cli -- <files>   Scan files"
          '';
        };
      });
}
