#!/bin/sh
# Generated from build.rcl. Do not edit.

# Run from the repo root.
set -euo pipefail

LIBZ_SYS_STATIC=1 cargo zigbuild --target=aarch64-apple-darwin --release
LIBZ_SYS_STATIC=1 cargo zigbuild --target=aarch64-unknown-linux-musl --release
LIBZ_SYS_STATIC=1 cargo zigbuild --target=armv7-unknown-linux-musleabihf --release
LIBZ_SYS_STATIC=1 cargo zigbuild --target=x86_64-unknown-linux-musl --release

VERSION=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
COMMIT=$(git rev-parse HEAD | cut -c-10)
BIN="deptool-$VERSION-$COMMIT"
DEST=target/deptool-bin

mkdir -p "$DEST/darwin-arm64"
cp 'target/aarch64-apple-darwin/release/deptool' "$DEST/darwin-arm64/$BIN"

mkdir -p "$DEST/linux-aarch64"
cp 'target/aarch64-unknown-linux-musl/release/deptool' "$DEST/linux-aarch64/$BIN"

mkdir -p "$DEST/linux-armv7l"
cp 'target/armv7-unknown-linux-musleabihf/release/deptool' "$DEST/linux-armv7l/$BIN"

mkdir -p "$DEST/linux-x86_64"
cp 'target/x86_64-unknown-linux-musl/release/deptool' "$DEST/linux-x86_64/$BIN"

