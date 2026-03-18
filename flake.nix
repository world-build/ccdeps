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

        # Test fixtures for integration tests (separate from Cargo source)
        testFixtures = ./tests/fixtures;

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ rustToolchain ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        cppscan = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          # Install the CLI binary from the workspace member
          cargoExtraArgs = "-p cppscan-cli";
        });

        cppscan-clippy = craneLib.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });

        cppscan-fmt = craneLib.cargoFmt {
          inherit src;
        };

        # Integration test: build a CMake project, generate compile_commands.json,
        # then verify cppscan produces the same deps as gcc -M.
        compdb-integration = pkgs.runCommand "cppscan-compdb-test" {
          nativeBuildInputs = [ cppscan pkgs.gcc pkgs.cmake pkgs.gnumake pkgs.diffutils ];
        } ''
          # Copy test project to writable directory
          cp -r ${testFixtures}/cmake-project project
          chmod -R u+w project

          # Run CMake to generate compile_commands.json
          mkdir project/build && cd project/build
          cmake .. -DCMAKE_EXPORT_COMPILE_COMMANDS=ON 2>&1

          # Run cppscan with the compilation database
          cppscan -p compile_commands.json -M > cppscan_out.txt

          # Run gcc -M with the same flags for each source file
          gcc -M -DUSE_FEATURE=1 -I../include ../src/main.c > gcc_main.txt 2>/dev/null
          gcc -M -DUSE_FEATURE=1 -I../include ../src/util.c > gcc_util.txt 2>/dev/null
          cat gcc_main.txt gcc_util.txt > gcc_out.txt

          # Parse dep outputs into sorted canonical dep lists
          # (strip targets, system headers, canonicalize paths for comparison)
          parse_deps() {
            cat "$1" \
              | tr '\\' ' ' | tr '\n' ' ' | sed 's/[^ ]*\.o://g' \
              | tr -s ' ' '\n' | grep -v '^$' \
              | grep -v '/nix/store/' \
              | while read -r p; do
                  # Resolve to canonical path, then strip the build prefix
                  # to get a project-relative comparison
                  realpath -m "$p" 2>/dev/null || echo "$p"
                done \
              | sed "s|^$(realpath /build/project)/||" \
              | sort -u
          }

          parse_deps cppscan_out.txt > cppscan_deps.txt
          parse_deps gcc_out.txt > gcc_deps.txt

          echo "=== cppscan deps ==="
          cat cppscan_deps.txt
          echo "=== gcc deps ==="
          cat gcc_deps.txt
          echo "=== diff ==="

          diff cppscan_deps.txt gcc_deps.txt
          echo "Integration test passed!"
          touch $out
        '';

      in {
        packages = {
          default = cppscan;
          inherit cppscan;
        };

        checks = {
          inherit cppscan cppscan-clippy cppscan-fmt compdb-integration;
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
