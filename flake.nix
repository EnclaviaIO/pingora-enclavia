{
  description = "pingora-enclavia: attested tunnel proxy built on Pingora";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.stable."1.88.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
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
      });
}
