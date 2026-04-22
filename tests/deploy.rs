//! Integration test: deploy locally, verify result on disk.
//!
//! Exercises the full binary end-to-end: CLI parsing, plan construction,
//! process spawning, the agent's JSON protocol, and file checkout — without
//! poking into any internals.

use std::fs;
use std::process::Command;

use deptool::testutil::TempDir;

const DEPTOOL: &str = env!("CARGO_BIN_EXE_deptool");

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
    let store = TempDir::new("store");
    let remote_store = TempDir::new("remote-store");
    let apps = TempDir::new("apps");
    let units = TempDir::new("units");
    let config = TempDir::new("config");

    let hostname = "testhost";
    fs::create_dir_all(config.path().join("testhost/nginx")).unwrap();
    fs::write(config.path().join("testhost/nginx/nginx.conf"), "server {}").unwrap();

    // Deploy locally.
    let output = Command::new(DEPTOOL)
        .args(["deploy", "--store"])
        .arg(store.path())
        .arg(config.path())
        .arg("--remote-store")
        .arg(remote_store.path())
        .args(["--local", "--no-confirm"])
        .env("DEPTOOL_HOSTNAME", hostname)
        .env("DEPTOOL_APPS_DIR", apps.path())
        .env("DEPTOOL_UNIT_DIR", units.path())
        .output()
        .expect("deptool runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deploy failed:\nstdout: {stdout}\nstderr: {stderr}",
    );

    let current = apps.path().join("nginx/current");
    assert!(current.is_symlink(), "current symlink should exist");
    assert_eq!(
        fs::read_to_string(current.join("nginx.conf")).expect("nginx.conf is readable"),
        "server {}",
    );
}

#[test]
fn deploy_rejects_invalid_config() {
    let store = TempDir::new("store");
    let config = TempDir::new("config");
    fs::create_dir_all(config.path().join("deckard/nginx")).unwrap();
    fs::write(
        config.path().join("deckard/nginx/manifest.json"),
        r#"{"unknown_key": true}"#,
    )
    .unwrap();

    let output = Command::new(DEPTOOL)
        .args(["deploy", "--store"])
        .arg(store.path())
        .arg(config.path())
        .args(["--plan-only"])
        .output()
        .expect("deptool runs");
    assert!(
        !output.status.success(),
        "deploy should reject invalid config"
    );
}
