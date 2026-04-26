use std::io::Write;
use std::process::Command;

#[test]
fn check_json_reports_machine_readable_diagnostics() {
    let mut spec = tempfile::NamedTempFile::new().expect("temp file");
    write!(
        spec,
        "{}",
        "GET /x\nResponse-Type text/plain\nExec: echo [%missing]\n"
    )
    .expect("write spec");

    let output = Command::new(env!("CARGO_BIN_EXE_mii-http"))
        .args(["--check", "--json", spec.path().to_str().unwrap()])
        .output()
        .expect("run mii-http");

    assert!(!output.status.success(), "invalid spec should fail");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be JSON");

    assert_eq!(report["ok"], false);
    assert_eq!(report["error_count"], 1);
    assert_eq!(
        report["diagnostics"][0]["message"],
        "unresolved reference: query param `missing`"
    );
    assert_eq!(report["diagnostics"][0]["span"]["start_line"], 2);
}
