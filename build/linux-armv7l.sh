#!/bin/sh
# Generated from build.rcl. Do not edit.

# Run from the repo root.
set -euo pipefail

VERSION=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
COMMIT=$(git rev-parse HEAD | cut -c-10)

LIBZ_SYS_STATIC=1 cargo zigbuild --target=armv7-unknown-linux-musleabihf --release

mkdir -p "target/deptool-bin/linux-armv7l"
cp "target/armv7-unknown-linux-musleabihf/release/deptool" "target/deptool-bin/linux-armv7l/deptool-$VERSION-$COMMIT"
