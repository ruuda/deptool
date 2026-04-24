// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Integration tests for deploy and sync.
//!
//! Exercises the full binary end-to-end: CLI parsing, plan construction,
//! process spawning, the agent's JSON protocol, and file checkout — without
//! poking into any internals.

use std::fs;
use std::process::Command;

use deptool::testutil::TempDir;

const DEPTOOL: &str = env!("CARGO_BIN_EXE_deptool");

/// Test environment for running deptool commands locally.
struct LocalEnv {
    store: TempDir,
    remote_store: TempDir,
    apps: TempDir,
    units: TempDir,
    config: TempDir,
    hostname: &'static str,
}

impl LocalEnv {
    fn new(hostname: &'static str) -> Self {
        Self {
            store: TempDir::new("store"),
            remote_store: TempDir::new("remote-store"),
            apps: TempDir::new("apps"),
            units: TempDir::new("units"),
            config: TempDir::new("config"),
            hostname,
        }
    }

    /// Write a file under `{hostname}/{path}` in the config directory.
    fn write_config(&self, path: &str, content: &[u8]) {
        let full = self.config.path().join(self.hostname).join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    /// Build a deptool command with the store and config dir set.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(DEPTOOL);
        cmd.args(args)
            .arg("--store")
            .arg(self.store.path())
            .arg(self.config.path());
        cmd
    }

    /// Run a deptool subcommand locally with the agent environment set.
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args)
            .arg("--remote-store")
            .arg(self.remote_store.path())
            .arg("--local")
            .env("DEPTOOL_HOSTNAME", self.hostname)
            .env("DEPTOOL_APPS_DIR", self.apps.path())
            .env("DEPTOOL_UNIT_DIR", self.units.path())
            .output()
            .expect("deptool runs")
    }
}

#[test]
fn help_does_not_crash() {
    let output = Command::new(DEPTOOL)
        .arg("--help")
        .output()
        .expect("deptool runs");
    assert!(output.status.success(), "--help exits successfully");
}

#[test]
fn deploy_locally() {
    let env = LocalEnv::new("testhost");
    env.write_config("nginx/nginx.conf", b"server {}");

    let output = env.run(&["deploy", "--no-confirm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deploy failed:\nstdout: {stdout}\nstderr: {stderr}",
    );

    let current = env.apps.path().join("nginx/current");
    assert!(current.is_symlink(), "current symlink should exist");
    assert_eq!(
        fs::read_to_string(current.join("nginx.conf")).expect("nginx.conf is readable"),
        "server {}",
    );
}

#[test]
fn sync_then_deploy_avoids_staleness() {
    let env = LocalEnv::new("testhost");

    let deploy = |version: &str| {
        env.write_config("nginx/nginx.conf", version.as_bytes());
        env.run(&["deploy", "--no-confirm"])
    };

    // Deploy v1, then v2.
    assert!(deploy("v1").status.success(), "v1 deploy failed");
    assert!(deploy("v2").status.success(), "v2 deploy failed");

    // Revert the tracking ref to simulate another operator deploying
    // v2 behind our back while our store still thinks v1 is current.
    let repo = git2::Repository::open(env.store.path()).unwrap();
    let reflog = repo.reflog("refs/remotes/testhost/current").unwrap();
    let v1_ref = reflog.get(1).expect("reflog has a previous entry").id_new();
    repo.reference(
        "refs/remotes/testhost/current",
        v1_ref,
        true,
        "revert for test",
    )
    .unwrap();

    // Sync fixes the stale ref. Deploy v3 should succeed.
    env.write_config("nginx/nginx.conf", b"v3");
    let output = env.run(&["sync"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sync failed: {stderr}");

    let output = env.run(&["deploy", "--no-confirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deploy after sync failed: {stderr}"
    );
}

#[test]
fn deploy_rejects_invalid_config() {
    let env = LocalEnv::new("deckard");
    env.write_config("nginx/manifest.json", br#"{"unknown_key": true}"#);

    // No --local needed: --plan-only exits before connecting to hosts.
    let output = env
        .cmd(&["deploy", "--plan-only"])
        .output()
        .expect("deptool runs");
    assert!(
        !output.status.success(),
        "deploy should reject invalid config"
    );
}
