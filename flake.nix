{
  description = "pingora-enclavia: attested tunnel proxy built on Pingora";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # SDK flake. Cargo.toml uses absolute path deps to `enclavia` and
    # `enclavia-protocol` (so plain `cargo build` keeps working in the
    # dev shell); the Nix build patches them onto this input's store
    # path at build time. Pinned by ref so the lockfile holds the rev —
    # bump alongside an SDK release.
    #
    # Tracking the `sdk-extract-open-stream` branch until enclavia#19
    # merges; flip back to `master` after that lands.
    enclavia = {
      url = "github:EnclaviaIO/enclavia/sdk-extract-open-stream";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, enclavia }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.stable."1.88.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # Patch Cargo.toml so the SDK path deps point at the flake
        # input's store path. The dev-shell build keeps using the
        # absolute /home/afilini/workspace/enclavia path (so iterating
        # locally needs zero flake updates); only the packaged build
        # gets the rewrite.
        pingoraEnclavia = rustPlatform.buildRustPackage {
          pname = "pingora-enclavia";
          version = "0.0.1";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = { };
          };

          nativeBuildInputs = with pkgs; [ pkg-config cmake perl ];
          buildInputs = with pkgs; [ openssl ];

          # Repoint absolute path deps onto the flake input. Done after
          # the source is unpacked so the file we edit is local to the
          # build, not the read-only flake source. Cargo.lock entries
          # for path deps carry no hash so no lock adjustment is needed.
          postPatch = ''
            substituteInPlace Cargo.toml \
              --replace "/home/afilini/workspace/enclavia/enclavia-protocol" "${enclavia}/enclavia-protocol" \
              --replace "/home/afilini/workspace/enclavia/enclavia" "${enclavia}/enclavia"
          '';

          # The noise-echo dev binary depends on the test_responder
          # module; building it inflates the closure for no production
          # value. Restrict the build to the proxy binary.
          cargoBuildFlags = [ "--bin" "pingora-enclavia" ];
          # Tests need network for the echo loopback path, which the
          # sandbox doesn't allow. We exercise tests via `cargo test`
          # in the dev shell; the packaged build is binary-only.
          doCheck = false;
        };

      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            pkg-config
            openssl
            cmake
            perl
            curl
            git
          ];
        };

        packages = {
          default = pingoraEnclavia;
          pingora-enclavia = pingoraEnclavia;
        };
      });
}
