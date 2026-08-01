//! The library error type.
//!
//! Errors are self-contained: they carry the offending file's name, the
//! source location (line, column, and the source line itself for context),
//! and any notes — nothing needs to be threaded back in to display them.
//! `Display` renders the classic caret diagnostic; `core::error::Error` is
//! implemented so errors compose with `?` into callers' error chains.

use crate::span::{LineIndex, Span};
use std::fmt;

/// A resolved source position, self-contained for rendering.
#[derive(Debug, Clone)]
pub struct Location {
    /// Import path / display name of the file, when known.
    pub file: Option<String>,
    /// 1-based.
    pub line: u32,
    /// 1-based.
    pub col: u32,
    /// Byte span in the original source.
    pub span: Span,
    /// The source line the span starts on, for caret rendering.
    pub line_text: String,
}

impl Location {
    pub(crate) fn resolve(span: Span, src: &str) -> Self {
        let idx = LineIndex::new(src);
        let (line, col) = idx.line_col(span.start);
        Self {
            file: None,
            line,
            col,
            span,
            line_text: idx.line_text(src, line).to_string(),
        }
    }
}

/// A secondary message attached to an [`Error`], optionally located.
#[derive(Debug, Clone)]
pub struct Note {
    /// The note's message.
    pub message: String,
    /// Where the note points, when it points anywhere.
    pub location: Option<Location>,
}

/// An error from any pbpp stage, with everything needed to report it.
///
/// The payload is boxed: `Error` rides the `Err` side of every hot
/// pipeline `Result` (per-symbol, per-reference calls), and one pointer
/// keeps those returns register-sized instead of a 112-byte memory return.
#[derive(Debug, Clone)]
pub struct Error {
    inner: Box<Inner>,
}

#[derive(Debug, Clone)]
struct Inner {
    message: String,
    location: Option<Location>,
    notes: Vec<Note>,
}

impl Error {
    /// An error with a message and no source location.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            inner: Box::new(Inner {
                message: message.into(),
                location: None,
                notes: Vec::new(),
            }),
        }
    }

    /// An error at a span, resolved against the source it points into.
    pub(crate) fn at(message: impl Into<String>, span: Span, src: &str) -> Self {
        Self {
            inner: Box::new(Inner {
                message: message.into(),
                location: Some(Location::resolve(span, src)),
                notes: Vec::new(),
            }),
        }
    }

    /// Attaches an unlocated note.
    #[must_use]
    pub fn note(mut self, message: impl Into<String>) -> Self {
        self.inner.notes.push(Note {
            message: message.into(),
            location: None,
        });
        self
    }

    #[must_use]
    pub(crate) fn note_at(mut self, message: impl Into<String>, span: Span, src: &str) -> Self {
        self.inner.notes.push(Note {
            message: message.into(),
            location: Some(Location::resolve(span, src)),
        });
        self
    }

    /// A note pointing into a *different* file than the error's own; the
    /// explicit name keeps `with_file` from overwriting it.
    #[must_use]
    pub(crate) fn note_at_file(
        mut self,
        message: impl Into<String>,
        span: Span,
        src: &str,
        file: &str,
    ) -> Self {
        let mut loc = Location::resolve(span, src);
        loc.file = Some(file.to_string());
        self.inner.notes.push(Note {
            message: message.into(),
            location: Some(loc),
        });
        self
    }

    /// Names the file for the error's location and for any note location
    /// that doesn't already have one. Locations resolved against other
    /// files keep their names.
    #[must_use]
    pub fn with_file(mut self, name: &str) -> Self {
        if let Some(loc) = &mut self.inner.location
            && loc.file.is_none()
        {
            loc.file = Some(name.to_string());
        }
        for n in &mut self.inner.notes {
            if let Some(loc) = &mut n.location
                && loc.file.is_none()
            {
                loc.file = Some(name.to_string());
            }
        }
        self
    }

    /// The primary message, without location or notes.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// Attached notes, in the order they were added.
    #[must_use]
    pub fn notes(&self) -> &[Note] {
        &self.inner.notes
    }

    /// 1-based line of the primary location, if any.
    #[must_use]
    pub fn line(&self) -> Option<u32> {
        self.inner.location.as_ref().map(|l| l.line)
    }
}

fn render_location(f: &mut fmt::Formatter<'_>, loc: &Location) -> fmt::Result {
    let file = loc.file.as_deref().unwrap_or("<input>");
    writeln!(f, "  --> {file}:{}:{}", loc.line, loc.col)?;
    let gutter = loc.line.to_string();
    let pad = " ".repeat(gutter.len());
    writeln!(f, " {pad} |")?;
    writeln!(f, " {gutter} | {}", loc.line_text)?;
    // Caret width: the span clamped to this line.
    let col = loc.col as usize;
    let line_rest = loc.line_text.len().saturating_sub(col - 1).max(1);
    let width = ((loc.span.end.saturating_sub(loc.span.start)).max(1) as usize).min(line_rest);
    writeln!(f, " {pad} | {}{}", " ".repeat(col - 1), "^".repeat(width))
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "error: {}", self.inner.message)?;
        if let Some(loc) = &self.inner.location {
            render_location(f, loc)?;
        }
        for note in &self.inner.notes {
            writeln!(f, "note: {}", note.message)?;
            if let Some(loc) = &note.location {
                render_location(f, loc)?;
            }
        }
        Ok(())
    }
}

impl core::error::Error for Error {}
