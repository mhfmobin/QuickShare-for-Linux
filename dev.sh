#!/bin/sh
# Run a command in the isolated build container, e.g. ./dev.sh cargo test
# Only the project dir is mounted; caches live in docker volumes.
[ -t 0 ] && TTY=-it
docker build -q -t qs-linux-dev . >/dev/null && exec docker run --rm $TTY \
  -u "$(id -u):$(id -g)" -v "$PWD":/src -v qs-cargo-registry:/opt/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target -e CARGO_TERM_COLOR=never qs-linux-dev "$@"
