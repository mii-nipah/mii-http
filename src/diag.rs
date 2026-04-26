//! Diagnostic helpers using ariadne.

use ariadne::{Color, Label, Report, ReportKind, Source};
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

    pub fn warning(message: impl Into<String>, span: Range<usize>, label: impl Into<String>) -> Self {
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
        let _ = builder
            .finish()
            .eprint((file_name, Source::from(source)));
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
