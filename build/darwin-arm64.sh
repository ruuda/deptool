#!/bin/sh
# Generated from build.rcl. Do not edit.

# Run from the repo root.
set -euo pipefail

VERSION=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
COMMIT=$(git rev-parse HEAD | cut -c-10)

LIBZ_SYS_STATIC=1 cargo zigbuild --target=aarch64-apple-darwin --release

mkdir -p "target/deptool-bin/darwin-arm64"
cp "target/aarch64-apple-darwin/release/deptool" "target/deptool-bin/darwin-arm64/deptool-$VERSION-$COMMIT"
ln -sf "deptool-$VERSION-$COMMIT" "target/deptool-bin/darwin-arm64/deptool"
