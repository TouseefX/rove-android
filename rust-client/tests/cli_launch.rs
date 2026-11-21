use serde_json::Value;
use std::process::Command;

#[test]
fn bare_launch_and_help_succeed_without_a_json_error() {
    let binary = env!("CARGO_BIN_EXE_rove");
    let bare = Command::new(binary).output().unwrap();
    assert!(bare.status.success());
    assert!(bare.stderr.is_empty());
    let message = String::from_utf8(bare.stdout).unwrap();
    assert!(message.contains("rove --help"));
    assert!(message.contains("rove-stellar.exe"));
    for flag in ["--help", "-h", "help"] {
        let help = Command::new(binary).arg(flag).output().unwrap();
        assert!(help.status.success());
        assert!(help.stderr.is_empty());
        let help_text = String::from_utf8(help.stdout).unwrap();
        assert!(help_text.lines().any(|line| line.starts_with("rove run --snapshot")));
    }
    let invalid = Command::new(binary).arg("missing-command").output().unwrap();
    assert!(!invalid.status.success());
    let error: Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(error["ok"], false);
}
