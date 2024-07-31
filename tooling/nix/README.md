# Nix Package

## How it works

1. There is a patch that completely removes the download code from the node runtime.
2. The path to NodeJS is hardcoded via the `NODE_PATH` environment variable (at build time), which is set inside
   `./package.nix`.

## Nix Pins

The Nix "pins" (hashes of vendored git dependencies) are stored in `./pins.json`. These can be updated manually on a
machine with Nix installed by running `./update-pins.sh`. The Nix Github workflow for Nix will suggest updates to this
file if required.
