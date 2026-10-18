//! Validate deployment settings in subprocesses without changing the test process environment.

use std::process::{Command, Output};

fn check(settings: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pool-server"))
        .arg("check-config")
        .env_clear()
        .env("POOL_CATALOG", concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/models.json"))
        .env("POOL_BIND", "127.0.0.1:18399")
        .env("RUST_LOG", "info")
        .envs(settings.iter().copied())
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run pool-server check-config")
}

fn logs(output: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

#[test]
fn legacy_deployment_environment_is_accepted_and_warns_once_without_secrets() {
    let output = check(&[
        ("POOL_AUTH_MODE", "keys"),
        ("POOL_CLIENT_API_KEYS", "legacy-secret-one,legacy-secret-two"),
        ("POOL_MINER_AUTH", "open"),
        ("POOL_ADMIN_TOKEN", "admin-secret"),
        ("POOL_MAX_ACTIVE_PER_CLIENT", "48"),
        ("POOL_MAX_QUEUED_PER_CLIENT", "64"),
    ]);
    let logs = logs(&output);
    assert!(output.status.success(), "{logs}");
    assert_eq!(logs.matches("POOL_CLIENT_API_KEYS is no longer used and is ignored").count(), 1, "{logs}");
    for secret in ["legacy-secret-one", "legacy-secret-two", "admin-secret"] {
        assert!(!logs.contains(secret), "credentials must never be logged");
    }
}

#[test]
fn token_mode_still_requires_miner_tokens_even_with_legacy_settings() {
    for legacy in ["", "legacy-secret"] {
        let output =
            check(&[("POOL_AUTH_MODE", "keys"), ("POOL_MINER_AUTH", "token"), ("POOL_CLIENT_API_KEYS", legacy)]);
        assert!(!output.status.success());
        assert!(logs(&output).contains("needs POOL_MINER_TOKENS, or POOL_MINER_AUTH=open"));
    }
}

#[test]
fn keys_mode_accepts_miner_tokens_without_other_credentials() {
    let output = check(&[("POOL_AUTH_MODE", "keys"), ("POOL_MINER_TOKENS", "miner-secret")]);
    assert!(output.status.success(), "{}", logs(&output));
    assert!(!logs(&output).contains("POOL_CLIENT_API_KEYS"));
}

#[test]
fn local_mode_refuses_non_loopback_binds() {
    for bind in ["0.0.0.0:18399", "[::]:18399", "192.0.2.1:18399"] {
        let output = check(&[("POOL_AUTH_MODE", "local"), ("POOL_BIND", bind)]);
        assert!(!output.status.success(), "{bind}");
        assert!(logs(&output).contains("POOL_AUTH_MODE=local only serves loopback"));
    }
}

#[test]
fn local_mode_accepts_loopback_without_miner_tokens() {
    for bind in ["127.0.0.1:18399", "[::1]:18399"] {
        let output = check(&[("POOL_AUTH_MODE", "local"), ("POOL_BIND", bind)]);
        assert!(output.status.success(), "{}", logs(&output));
    }
}
