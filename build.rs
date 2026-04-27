// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Bake build identity into the binary as `BUILD_COMMIT`, `BUILD_COMMIT_DATE`,
//! and `BUILD_PLATFORM`.
//!
//! `BUILD_COMMIT` is the suffix in the installed binary name
//! (`deptool-{VERSION}-{COMMIT[:10]}`). It identifies content at the source
//! level rather than the binary level, so per-target binaries built from the
//! same source tree share a name on the target host. Release builds refuse
//! to start from a dirty tree, so the suffix is unambiguous in practice.
//!
//! `BUILD_COMMIT_DATE` is the committer date of `BUILD_COMMIT` in `YYYY-MM-DD`
//! form. Shown in `--version` so the user can tell at a glance how old the
//! binary is without resolving the commit hash.
//!
//! `BUILD_PLATFORM` is the `uname -sm` output the binary's target prints,
//! e.g. "Linux x86_64". It lets the driver short-circuit the binaries-cache
//! lookup when deploying to a host of the same platform: the operator's own
//! binary works, no separate cache entry needed.

use std::process::Command;

#[path = "build/build_platform.rs"]
mod build_platform;
use build_platform::build_platform_for;

fn main() {
    // Re-run when the commit moves, when staged changes are added or
    // removed, when source changes, or when this script changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/build_platform.rs");

    // Map the cargo target triple to its `uname -sm` output via the
    // generated `build_platform_for` function. Used to skip the
    // binaries-cache lookup when deploying to a host of the same platform
    // as the operator's binary.
    let target = std::env::var("TARGET").expect("TARGET is set by Cargo");
    let build_platform = build_platform_for(&target);
    println!("cargo:rustc-env=BUILD_PLATFORM={build_platform}");

    // If BUILD_COMMIT is not set, then we use Git to look up the current
    // commit below. This is the default case for local development. We allow
    // bypassing Git by setting the env var, so that the application can be
    // built as a Nix flake as well. In that case the caller must also supply
    // BUILD_COMMIT_DATE, and we assume the tree is not dirty.
    if std::env::var("BUILD_COMMIT").is_ok() {
        return;
    }

    let head = Command::new("git")
        .args(["show", "-s", "--format=%H %cs", "HEAD"])
        .output()
        .expect("git is installed");
    if !head.status.success() {
        panic!(
            "git show HEAD failed: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        );
    }
    let head = std::str::from_utf8(&head.stdout)
        .expect("git show output is valid UTF-8")
        .trim();
    let (commit, date) = head
        .split_once(' ')
        .expect("git show prints commit and date separated by a space");
    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_COMMIT_DATE={date}");

    // Release binaries get pushed to target hosts, where stale or ambiguous
    // identity is a real footgun. Refuse to build a release from a dirty tree
    // so the commit suffix is always trustworthy. We compare the working tree
    // to HEAD (covering staged and unstaged changes) but ignore untracked
    // files, which often contain personal scratch.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        let diff = Command::new("git")
            .args(["diff", "--quiet", "HEAD"])
            .status()
            .expect("git is installed");
        match diff.code() {
            Some(0) => {}
            Some(1) => panic!(
                "Refusing to build a release binary from a dirty tree.\n\
                 Commit or stash changes first."
            ),
            _ => panic!("git diff --quiet HEAD failed"),
        }
    }
}
