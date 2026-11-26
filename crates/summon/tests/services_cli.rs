//! Integration test for `summon services` CLI error sanitization.

use std::process::Command;

const SUMMON: &str = env!("CARGO_BIN_EXE_summon");

#[test]
fn cli_services_check_on_broken_config_never_leaks_scalars_to_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let home_dir = tmp.path().join(".summon");
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::write(
        home_dir.join("config.yaml"),
        "services:\n  synth:\n    healthcheck:\n      tcp: 127.0.0.1:18999\n    survive_daemon_restart: synth-cli-stderr-secret-3131\n",
    )
    .unwrap();
    let out = Command::new(SUMMON)
        .args(["services", "check"])
        .env("HOME", tmp.path())
        .env("SUMMON_HOME", &home_dir)
        .output()
        .expect("spawn summon binary");
    assert!(
        !out.status.success(),
        "services check must exit non-zero on broken config"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stderr.contains("synth-cli-stderr-secret-3131")
            && !stdout.contains("synth-cli-stderr-secret-3131"),
        "CLI output must never leak scalar values: stdout={stdout:?} stderr={stderr:?}"
    );
}
