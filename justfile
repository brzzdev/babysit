# prettier-ignore

[private]
default:
  just --list

# Build a release binary.
build-release:
  cargo build --release

# Format the source code.
format:
  cargo fmt

# Check the source code is formatted, without rewriting it. Mirrors CI.
format-check:
  cargo fmt --check

# Install the release binary locally.
install: build-release
  mkdir -p ~/.local/bin
  @# `install` writes a fresh file where `cp` would overwrite in place, leaving
  @# macOS a stale code signature that kills the new binary on launch.
  install target/release/babysit ~/.local/bin/babysit

# Lint the source code.
lint:
  cargo clippy --all-targets -- -D warnings

# Run the test suite.
test:
  cargo test
