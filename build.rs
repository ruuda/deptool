// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Bake the current Git commit into the binary as `BUILD_COMMIT`.
//!
//! The commit hash is used as the suffix in the installed binary name
//! (`deptool-{VERSION}-{COMMIT[:10]}`). It identifies content at the source
//! level rather than the binary level, so per-target binaries built from the
//! same source tree share a name on the target host. Release builds refuse
//! to start from a dirty tree, so the suffix is unambiguous in practice.

use std::process::Command;

fn main() {
    // Re-run when the commit moves, when staged changes are added or
    // removed, when source changes, or when this script changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git is installed");
    if !head.status.success() {
        panic!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        );
    }
    let commit = std::str::from_utf8(&head.stdout)
        .expect("git rev-parse output is valid UTF-8")
        .trim();
    println!("cargo:rustc-env=BUILD_COMMIT={commit}");

    // Release binaries get pushed to target hosts, where stale or ambiguous
    // identity is a real footgun. Refuse to build a release from a dirty
    // tree so the commit suffix is always trustworthy. We compare the
    // working tree to HEAD (covering staged and unstaged changes) but
    // ignore untracked files, which often contain personal scratch.
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
