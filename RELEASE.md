# Release

## Cutting a new version

1. Bump `version` in `Cargo.toml` and `flake.nix` (search for `version = "0.1.0"`).
2. Run `cargo update -p pingora-enclavia` to refresh `Cargo.lock`.
3. Commit and tag (`git tag v0.1.0 && git push --tags`).

## Publishing the Docker image

The image is built reproducibly by Nix; no Docker daemon is needed during the build.

```bash
nix build .#dockerImage
docker load < result
docker push enclaviaio/pingora-enclavia:0.1.0
docker tag enclaviaio/pingora-enclavia:0.1.0 enclaviaio/pingora-enclavia:latest
docker push enclaviaio/pingora-enclavia:latest
```

Requires `docker login` against Docker Hub with credentials that can push under the `enclaviaio` namespace.

## Verifying the build

```bash
nix build .#pingora-enclavia
./result/bin/pingora-enclavia --help
```
