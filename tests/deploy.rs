//! Integration test: commit files, deploy locally, verify result on disk.
//!
//! Exercises the full binary end-to-end: CLI parsing, plan construction,
//! process spawning, the agent's JSON protocol, and file checkout — without
//! poking into any internals.

use std::fs;
use std::process::Command;

use deptool::testutil::TempDir;

const DEPTOOL: &str = env!("CARGO_BIN_EXE_deptool");

#[test]
fn commit_and_deploy_locally() {
    let store = TempDir::new("store");
    let remote_store = TempDir::new("remote-store");
    let apps = TempDir::new("apps");
    let units = TempDir::new("units");

    // Create a config directory to commit. The host directory must match
    // the agent's hostname, which we control via DEPTOOL_HOSTNAME.
    let hostname = "testhost";
    let config = TempDir::new("config");
    fs::create_dir_all(config.path().join(&hostname).join("nginx")).unwrap();
    fs::write(
        config.path().join(&hostname).join("nginx/nginx.conf"),
        "server {}",
    )
    .unwrap();

    // Commit the config.
    let output = Command::new(DEPTOOL)
        .args(["commit", "--store"])
        .arg(store.path())
        .arg(config.path())
        .output()
        .expect("deptool commit runs");
    assert!(
        output.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Deploy locally.
    let output = Command::new(DEPTOOL)
        .args(["deploy", "--store"])
        .arg(store.path())
        .arg("--remote-store")
        .arg(remote_store.path())
        .args(["--local", "--no-confirm"])
        .env("DEPTOOL_HOSTNAME", hostname)
        .env("DEPTOOL_APPS_DIR", apps.path())
        .env("DEPTOOL_UNIT_DIR", units.path())
        .output()
        .expect("deptool deploy runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deploy failed:\nstdout: {stdout}\nstderr: {stderr}",
    );

    // Verify the app was checked out on the target.
    let nginx_dir = apps.path().join("nginx");
    assert!(nginx_dir.exists(), "nginx app dir should exist");
    let current = nginx_dir.join("current");
    assert!(current.is_symlink(), "current symlink should exist");
    let conf_path = current.join("nginx.conf");
    assert_eq!(
        fs::read_to_string(&conf_path).expect("nginx.conf is readable"),
        "server {}",
    );
}

#[test]
fn commit_skips_when_tree_unchanged() {
    let store = TempDir::new("store");
    let config = TempDir::new("config");
    let host_dir = config.path().join("deckard/nginx");
    fs::create_dir_all(&host_dir).unwrap();
    fs::write(host_dir.join("nginx.conf"), "server {}").unwrap();

    // First commit creates the ref.
    let first = Command::new(DEPTOOL)
        .args(["commit", "--store"])
        .arg(store.path())
        .arg(config.path())
        .output()
        .expect("deptool commit runs");
    assert!(first.status.success());

    // Second commit with the same tree should not create a new commit.
    let second = Command::new(DEPTOOL)
        .args(["commit", "--store"])
        .arg(store.path())
        .arg(config.path())
        .output()
        .expect("deptool commit runs");
    assert!(second.status.success());
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("No changes"),
        "expected no-changes message: {stdout}"
    );
}

#[test]
fn commit_rejects_invalid_config() {
    let store = TempDir::new("store");
    let config = TempDir::new("config");
    let app_dir = config.path().join("deckard/nginx");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(app_dir.join("manifest.json"), r#"{"unknown_key": true}"#).unwrap();

    let output = Command::new(DEPTOOL)
        .args(["commit", "--store"])
        .arg(store.path())
        .arg(config.path())
        .output()
        .expect("deptool commit runs");
    assert!(
        !output.status.success(),
        "commit should reject invalid config"
    );
}
