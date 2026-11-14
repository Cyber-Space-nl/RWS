use std::process::Command;

#[test]
fn unified_binary_reports_its_name_and_default_config() {
    let binary = env!("CARGO_BIN_EXE_rws");
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("rws {}", env!("CARGO_PKG_VERSION"))
    );
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("/etc/rws/config.toml"));
    assert!(!help.contains("rws-simple") && !help.contains("rws-comfy"));
}
