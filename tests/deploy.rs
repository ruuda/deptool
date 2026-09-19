// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Integration tests for deploy, sync, and reapply.
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
        let env = Self {
            store: TempDir::new("store"),
            remote_store: TempDir::new("remote-store"),
            apps: TempDir::new("apps"),
            units: TempDir::new("units"),
            config: TempDir::new("config"),
            hostname,
        };
        let output = Command::new(DEPTOOL)
            .arg("init")
            .env("DEPTOOL_STORE", env.store.path())
            .output()
            .expect("deptool runs");
        assert!(output.status.success(), "init exits successfully");
        env
    }

    /// Write a file under `{hostname}/{path}` in the config directory.
    fn write_config(&self, path: &str, content: &[u8]) {
        let full = self.config.path().join(self.hostname).join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    /// The config tree directory as a string, for inclusion in `run` args.
    fn config_dir(&self) -> &str {
        self.config
            .path()
            .to_str()
            .expect("config path is valid UTF-8")
    }

    /// Run a deptool subcommand locally. The caller passes `config_dir()` in
    /// `args` to set the cluster explicitly, or omits it to exercise the
    /// recorded default.
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(DEPTOOL)
            .args(args)
            .arg("--local")
            .env("DEPTOOL_STORE", self.store.path())
            .env("DEPTOOL_HOSTNAME", self.hostname)
            .env("DEPTOOL_APPS_DIR", self.apps.path())
            .env("DEPTOOL_UNITS_DIR", self.units.path())
            .env("DEPTOOL_REMOTE_STORE", self.remote_store.path())
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
fn init_creates_bare_repo() {
    let store = TempDir::new("store");
    let output = Command::new(DEPTOOL)
        .arg("init")
        .env("DEPTOOL_STORE", store.path())
        .output()
        .expect("deptool runs");
    assert!(output.status.success(), "init exits successfully");
    git2::Repository::open_bare(store.path()).expect("init creates a bare repo");
}

#[test]
fn deploy_without_dir_uses_recorded_default() {
    let env = LocalEnv::new("testhost");
    env.write_config("nginx/nginx.conf", b"v1");

    let out = env.run(&["deploy", "--no-confirm", env.config_dir()]);
    assert!(out.status.success(), "initial deploy succeeds");

    env.write_config("nginx/nginx.conf", b"v2");
    let out = env.run(&["deploy", "--no-confirm"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "second deploy succeeds: {stderr}");

    let current = env.apps.path().join("nginx/current");
    assert_eq!(
        fs::read_to_string(current.join("nginx.conf")).expect("nginx.conf is readable"),
        "v2",
    );
}

#[test]
fn deploy_without_dir_or_default_names_the_problem() {
    let env = LocalEnv::new("testhost");
    let out = env.run(&["deploy"]);
    assert!(!out.status.success(), "deploy without dir or default fails");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no default cluster"),
        "error names the missing default, got: {stderr}",
    );
}

#[test]
fn deploy_with_missing_dir_fails_without_replacing_the_default() {
    let env = LocalEnv::new("testhost");
    let out = env.run(&["deploy", "--plan-only", env.config_dir()]);
    assert!(out.status.success(), "deploy with an existing dir succeeds");

    let out = env.run(&["deploy", "--plan-only", "no-such-dir"]);
    assert!(!out.status.success(), "deploy with a missing dir fails");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no config tree directory at 'no-such-dir'"),
        "error includes the missing path, got: {stderr}",
    );

    let out = env.run(&["deploy", "--plan-only"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "deploy without dir still uses the earlier default: {stderr}",
    );
}

#[test]
fn deploy_with_vanished_default_dir_error_includes_path_and_origin() {
    let env = LocalEnv::new("testhost");
    let gone = env.config.path().join("gone");
    fs::create_dir(&gone).unwrap();
    let out = env.run(&["deploy", "--plan-only", gone.to_str().unwrap()]);
    assert!(out.status.success(), "deploy with an existing dir succeeds");

    fs::remove_dir(&gone).unwrap();
    let out = env.run(&["deploy", "--plan-only"]);
    assert!(
        !out.status.success(),
        "deploy with a vanished default fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no config tree directory at '"),
        "error includes the vanished path, got: {stderr}",
    );
    assert!(
        stderr.contains("previously used"),
        "error explains where the path came from, got: {stderr}",
    );
}

#[test]
fn deploy_without_store_points_to_init() {
    let config = TempDir::new("config");
    let output = Command::new(DEPTOOL)
        .arg("deploy")
        .arg(config.path())
        .env("DEPTOOL_STORE", "does-not-exist")
        .output()
        .expect("deptool runs");
    assert!(!output.status.success(), "deploy without store fails");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no store at 'does-not-exist'"),
        "error names the missing path, got: {stderr}",
    );
    assert!(
        stderr.contains("deptool init"),
        "error suggests 'deptool init', got: {stderr}",
    );
}

#[test]
fn deploy_locally() {
    let env = LocalEnv::new("testhost");
    env.write_config("nginx/nginx.conf", b"server {}");

    let output = env.run(&["deploy", "--no-confirm", env.config_dir()]);
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
fn up_to_date_message_covers_only_the_selection_under_limit() {
    let env = LocalEnv::new("testhost");
    env.write_config("nginx/nginx.conf", b"v1");
    let out = env.run(&["deploy", "--no-confirm", env.config_dir()]);
    assert!(out.status.success(), "initial deploy succeeds");

    let out = env.run(&["deploy", "--plan-only"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("All hosts in cluster"),
        "without --limit the message covers the whole cluster, got: {stderr}",
    );

    // Give a second host pending changes. A --limit run that excludes it
    // must not claim the whole cluster is up to date.
    let other = env.config.path().join("otherhost/nginx");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("nginx.conf"), b"v1").unwrap();

    let out = env.run(&["deploy", "--plan-only", "--limit", "testhost"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("All selected hosts in cluster"),
        "with --limit the message covers only the selection, got: {stderr}",
    );
}

#[test]
fn sync_then_deploy_avoids_staleness() {
    let env = LocalEnv::new("testhost");

    let deploy = |version: &str| {
        env.write_config("nginx/nginx.conf", version.as_bytes());
        env.run(&["deploy", "--no-confirm", env.config_dir()])
    };

    // Deploy v1, then v2.
    assert!(deploy("v1").status.success(), "v1 deploy failed");
    assert!(deploy("v2").status.success(), "v2 deploy failed");

    // Revert the tracking ref to simulate another operator deploying
    // v2 behind our back while our store still thinks v1 is current.
    let repo = git2::Repository::open(env.store.path()).unwrap();
    let reflog = repo.reflog("refs/remotes/testhost").unwrap();
    let v1_ref = reflog.get(1).expect("reflog has a previous entry").id_new();
    repo.reference("refs/remotes/testhost", v1_ref, true, "revert for test")
        .unwrap();

    // Sync fixes the stale ref. Deploy v3 should succeed.
    env.write_config("nginx/nginx.conf", b"v3");
    let output = env.run(&["sync", env.config_dir()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sync failed: {stderr}");

    let output = env.run(&["deploy", "--no-confirm", env.config_dir()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deploy after sync failed: {stderr}"
    );
}

// The stale round is covered by unit tests in agent.rs and deploy.rs;
// this exercises a rename end to end through the real binary: the first
// deploy after the rename records the host under its new name, the next
// one deploys to it.
#[test]
fn renamed_host_deploys_after_one_stale_round() {
    let mut env = LocalEnv::new("web1");
    env.write_config("nginx/nginx.conf", b"v1");
    let out = env.run(&["deploy", "--no-confirm", env.config_dir()]);
    assert!(out.status.success(), "initial deploy succeeds");

    // The machine is renamed: the hostname and the config tree directory
    // change, the deployed state stays.
    env.hostname = "web2";
    fs::rename(
        env.config.path().join("web1"),
        env.config.path().join("web2"),
    )
    .unwrap();
    env.write_config("nginx/nginx.conf", b"v2");

    // The operator has no record of web2, so the plan expects a fresh
    // host; the deploy is stale and records the host's commit as web2.
    let out = env.run(&["deploy", "--no-confirm"]);
    assert!(
        !out.status.success(),
        "the first deploy after a rename is stale"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stale"), "got: {stdout}");

    let out = env.run(&["deploy", "--no-confirm"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the next deploy after a rename succeeds: {stderr}",
    );
    let current = env.apps.path().join("nginx/current");
    assert_eq!(
        fs::read_to_string(current.join("nginx.conf")).expect("nginx.conf is readable"),
        "v2",
    );
}

// Drift repair itself is covered by the unit tests in reapply.rs; this
// exercises the CLI wiring end to end through the real binary.
#[test]
fn reapply_locally() {
    let env = LocalEnv::new("testhost");
    env.write_config("nginx/nginx.conf", b"server {}");
    let out = env.run(&["deploy", "--no-confirm", env.config_dir()]);
    assert!(out.status.success(), "initial deploy succeeds");

    let out = env.run(&["reapply", "--no-confirm", env.config_dir()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "reapply failed:\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("Reapplied successfully to 1 host"),
        "summary shows the reapply, got: {stdout}",
    );
}

#[test]
fn deploy_rejects_invalid_config() {
    let env = LocalEnv::new("deckard");
    env.write_config("nginx/manifest.json", br#"{"unknown_key": true}"#);

    let output = env.run(&["deploy", "--plan-only", env.config_dir()]);
    assert!(
        !output.status.success(),
        "deploy should reject invalid config"
    );
}
