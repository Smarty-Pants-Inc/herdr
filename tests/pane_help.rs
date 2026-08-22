#[test]
fn pane_custom_help_lists_report_metadata() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["pane", "help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "herdr pane help failed: status={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("herdr pane report-metadata <pane_id> --source ID"),
        "pane help did not list report-metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
