use mii_http::diag::{self, Diag};

#[test]
fn json_report_keeps_span_and_line_column() {
    let source = "one\ntwo\n";
    let diagnostic = Diag::error("bad field", 6..7, "look here").with_note("use a safe type");

    let report = diag::report(&[diagnostic], source, Some(0));

    assert!(!report.ok);
    assert_eq!(report.error_count, 1);
    assert_eq!(report.warning_count, 0);
    assert_eq!(report.endpoint_count, Some(0));
    assert_eq!(report.diagnostics[0].kind, "error");
    assert_eq!(report.diagnostics[0].message, "bad field");
    assert_eq!(report.diagnostics[0].label, "look here");
    assert_eq!(
        report.diagnostics[0].note.as_deref(),
        Some("use a safe type")
    );
    assert_eq!(report.diagnostics[0].span.start, 6);
    assert_eq!(report.diagnostics[0].span.end, 7);
    assert_eq!(report.diagnostics[0].span.start_line, 1);
    assert_eq!(report.diagnostics[0].span.start_column, 2);
    assert_eq!(report.diagnostics[0].span.end_line, 1);
    assert_eq!(report.diagnostics[0].span.end_column, 3);
}
