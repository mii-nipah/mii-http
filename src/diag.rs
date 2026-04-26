//! Diagnostic helpers using ariadne.

use ariadne::{Color, Label, Report, ReportKind, Source};
use serde::Serialize;
use std::ops::Range;

#[derive(Debug, Clone)]
pub struct Diag {
    pub kind: DiagKind,
    pub message: String,
    pub label: String,
    pub span: Range<usize>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagKind {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub ok: bool,
    pub error_count: usize,
    pub warning_count: usize,
    pub endpoint_count: Option<usize>,
    pub diagnostics: Vec<JsonDiag>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonDiag {
    pub kind: &'static str,
    pub message: String,
    pub label: String,
    pub note: Option<String>,
    pub span: JsonSpan,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonSpan {
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

impl Diag {
    pub fn error(message: impl Into<String>, span: Range<usize>, label: impl Into<String>) -> Self {
        Self {
            kind: DiagKind::Error,
            message: message.into(),
            label: label.into(),
            span,
            note: None,
        }
    }

    pub fn warning(
        message: impl Into<String>,
        span: Range<usize>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            kind: DiagKind::Warning,
            message: message.into(),
            label: label.into(),
            span,
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn emit(&self, file_name: &str, source: &str) {
        let kind = match self.kind {
            DiagKind::Error => ReportKind::Error,
            DiagKind::Warning => ReportKind::Warning,
        };
        let span = clamp_span(&self.span, source.len());
        let mut builder = Report::build(kind, (file_name, span.clone()))
            .with_message(&self.message)
            .with_label(
                Label::new((file_name, span))
                    .with_message(&self.label)
                    .with_color(match self.kind {
                        DiagKind::Error => Color::Red,
                        DiagKind::Warning => Color::Yellow,
                    }),
            );
        if let Some(note) = &self.note {
            builder = builder.with_note(note);
        }
        let _ = builder.finish().eprint((file_name, Source::from(source)));
    }
}

fn clamp_span(span: &Range<usize>, max: usize) -> Range<usize> {
    let start = span.start.min(max);
    let end = span.end.min(max).max(start);
    start..end
}

pub fn emit_all(diags: &[Diag], file_name: &str, source: &str) {
    for d in diags {
        d.emit(file_name, source);
    }
}

pub fn report(diags: &[Diag], source: &str, endpoint_count: Option<usize>) -> DiagnosticReport {
    let diagnostics: Vec<_> = diags
        .iter()
        .map(|d| JsonDiag {
            kind: match d.kind {
                DiagKind::Error => "error",
                DiagKind::Warning => "warning",
            },
            message: d.message.clone(),
            label: d.label.clone(),
            note: d.note.clone(),
            span: json_span(&d.span, source),
        })
        .collect();
    let error_count = diagnostics.iter().filter(|d| d.kind == "error").count();
    let warning_count = diagnostics.iter().filter(|d| d.kind == "warning").count();
    DiagnosticReport {
        ok: error_count == 0,
        error_count,
        warning_count,
        endpoint_count,
        diagnostics,
    }
}

fn json_span(span: &Range<usize>, source: &str) -> JsonSpan {
    let span = clamp_span(span, source.len());
    let (start_line, start_column) = line_column(source, span.start);
    let (end_line, end_column) = line_column(source, span.end);
    JsonSpan {
        start: span.start,
        end: span.end,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn line_column(source: &str, byte_offset: usize) -> (usize, usize) {
    let byte_offset = previous_char_boundary(source, byte_offset.min(source.len()));
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    let column = source[line_start..byte_offset].chars().count();
    (line, column)
}

fn previous_char_boundary(source: &str, mut byte_offset: usize) -> usize {
    while byte_offset > 0 && !source.is_char_boundary(byte_offset) {
        byte_offset -= 1;
    }
    byte_offset
}
