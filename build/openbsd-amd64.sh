#!/bin/sh
# Generated from build.rcl. Do not edit.

# Run from the repo root.
set -euo pipefail

VERSION=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
COMMIT=$(git rev-parse HEAD | cut -c-10)

cargo zigbuild --target=x86_64-unknown-openbsd --release

mkdir -p "target/deptool-bin/openbsd-amd64"
cp "target/x86_64-unknown-openbsd/release/deptool" "target/deptool-bin/openbsd-amd64/deptool-$VERSION-$COMMIT"
