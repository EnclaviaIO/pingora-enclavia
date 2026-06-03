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
    let
      version = "0.1.0";
    in
    (flake-utils.lib.eachDefaultSystem (system:
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

        pingoraEnclavia = rustPlatform.buildRustPackage {
          pname = "pingora-enclavia";
          inherit version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs = with pkgs; [ pkg-config cmake perl ];
          buildInputs = with pkgs; [ openssl ];

          # The noise-echo dev binary depends on the test_responder
          # module; building it inflates the closure for no production
          # value. Restrict the build to the proxy binary.
          cargoBuildFlags = [ "--bin" "pingora-enclavia" ];
          doCheck = false;
        };

        dockerImage = pkgs.dockerTools.buildLayeredImage {
          name = "enclaviaio/pingora-enclavia";
          tag = version;
          contents = [
            pingoraEnclavia
            pkgs.cacert
            pkgs.dockerTools.caCertificates
          ];
          config = {
            Entrypoint = [ "${pingoraEnclavia}/bin/pingora-enclavia" ];
            Cmd = [
              "--config-dir"
              "/etc/pingora-enclavia/targets"
              "--listen"
              "0.0.0.0:6188"
            ];
            ExposedPorts = {
              "6188/tcp" = { };
            };
            Env = [
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
            ];
          };
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
          dockerImage = dockerImage;
        };
      })) // {
        nixosModules.default = { pkgs, lib, ... }: {
          imports = [ ./nix/module.nix ];
          config = lib.mkIf (self.packages ? ${pkgs.system}) {
            services.pingora-enclavia.package = lib.mkDefault self.packages.${pkgs.system}.pingora-enclavia;
          };
        };
      };
}
