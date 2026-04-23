//! Compiler Diagnostics
//!
//! Provides error and warning reporting throughout the compilation pipeline.
//! Diagnostics are accumulated rather than aborting on the first error,
//! allowing multiple issues to be reported in a single compilation run.
//!
//! # Usage
//!
//! ```ignore
//! let mut diags = Diagnostics::new();
//!
//! // Emit an error
//! diags.error(DiagnosticCode::E001_UndefinedVariable, span, "undefined variable `x`");
//!
//! // Emit with related notes
//! diags.error(DiagnosticCode::E010_TypeMismatch, use_span, "type mismatch")
//!     .note(def_span, "variable defined here");
//!
//! // Check for errors
//! if diags.has_errors() {
//!     // Report and abort
//! }
//! ```

use crate::ast::Span;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

// ============================================================================
// Severity
// ============================================================================

/// Severity level of a diagnostic
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational message (does not indicate a problem)
    Info,
    /// Warning (code is valid but may indicate a problem)
    Warning,
    /// Error (code is invalid, compilation cannot proceed)
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Info => write!(f, "info"),
            Severity::Warning => write!(f, "warning"),
            Severity::Error => write!(f, "error"),
        }
    }
}

// ============================================================================
// Diagnostic Codes
// ============================================================================

/// Diagnostic codes organized by compilation phase
///
/// Codes are numbered by category:
/// - E001-E099: Parsing errors
/// - E100-E199: Lowering errors (AST to IR)
/// - E200-E299: Definedness analysis errors
/// - E300-E399: Type analysis errors
/// - E400-E499: Semantic errors
/// - E500-E599: Linking errors
/// - W001-W099: Warnings
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCode {
    // ========================================================================
    // Parsing Errors (E001-E099)
    // ========================================================================
    /// Unexpected token in input
    E001_UnexpectedToken,
    /// Unclosed delimiter (parenthesis, bracket, brace)
    E002_UnclosedDelimiter,
    /// Invalid literal value
    E003_InvalidLiteral,
    /// Invalid escape sequence in string
    E004_InvalidEscape,

    // ========================================================================
    // Lowering Errors (E100-E199)
    // ========================================================================
    /// Reference to undefined variable
    E100_UndefinedVariable,
    /// Reference to undefined function
    E101_UndefinedFunction,
    /// Reference to undefined constant
    E102_UndefinedConstant,
    /// Break/continue outside of loop
    E103_InvalidLoopControl,
    /// Invalid assignment target (not an lvalue)
    E104_InvalidAssignmentTarget,
    /// Invalid pattern in context
    E105_InvalidPattern,
    /// Constant evaluation failed
    E106_ConstEvalFailed,

    // ========================================================================
    // Definedness Warnings (W200-W299)
    // ========================================================================
    /// Use of definitely undefined value
    W200_UseOfUndefined,
    /// Use of possibly undefined value without guard
    W201_UseOfMaybeUndefined,
    /// Assignment to undefined location
    W202_AssignmentToUndefined,

    // ========================================================================
    // Type Analysis Errors (E300-E399)
    // ========================================================================
    /// Type mismatch in operation
    E300_TypeMismatch,
    /// Invalid operand type for operator
    E301_InvalidOperandType,
    /// Invalid argument type for function
    E302_InvalidArgumentType,
    /// Invalid return type
    E303_InvalidReturnType,
    /// Cannot index into non-collection type
    E304_NotIndexable,

    // ========================================================================
    // Semantic Errors (E400-E499)
    // ========================================================================
    /// Duplicate definition
    E400_DuplicateDefinition,
    /// Invalid number of arguments
    E401_ArgumentCount,
    /// Unreachable code after return/break
    E402_UnreachableCode,
    /// Division by zero (in const eval)
    E403_DivisionByZero,
    /// Integer overflow (in const eval)
    E404_IntegerOverflow,

    // ========================================================================
    // Linking Errors (E500-E599)
    // ========================================================================
    /// Undefined external reference
    E500_UndefinedExternal,
    /// Missing entry point
    E501_MissingEntryPoint,
    /// Cyclic dependency
    E502_CyclicDependency,

    // ========================================================================
    // Warnings (W001-W099)
    // ========================================================================
    /// Unused variable
    W001_UnusedVariable,
    /// Unused function
    W002_UnusedFunction,
    /// Unreachable code
    W003_UnreachableCode,
    /// Shadowed variable
    W004_ShadowedVariable,
    /// Redundant guard (value is always defined)
    W005_RedundantGuard,
    /// Redundant type check (type is already known)
    W006_RedundantTypeCheck,
    /// Implicit conversion
    W007_ImplicitConversion,
    /// Deprecated feature
    W008_Deprecated,
    /// Operation on incompatible types (always produces undefined)
    W009_TypeMismatch,
}

impl DiagnosticCode {
    /// Get the string code (e.g., "E001", "W003")
    pub fn code(&self) -> &'static str {
        match self {
            // Parsing
            DiagnosticCode::E001_UnexpectedToken => "E001",
            DiagnosticCode::E002_UnclosedDelimiter => "E002",
            DiagnosticCode::E003_InvalidLiteral => "E003",
            DiagnosticCode::E004_InvalidEscape => "E004",

            // Lowering
            DiagnosticCode::E100_UndefinedVariable => "E100",
            DiagnosticCode::E101_UndefinedFunction => "E101",
            DiagnosticCode::E102_UndefinedConstant => "E102",
            DiagnosticCode::E103_InvalidLoopControl => "E103",
            DiagnosticCode::E104_InvalidAssignmentTarget => "E104",
            DiagnosticCode::E105_InvalidPattern => "E105",
            DiagnosticCode::E106_ConstEvalFailed => "E106",

            // Definedness
            DiagnosticCode::W200_UseOfUndefined => "W200",
            DiagnosticCode::W201_UseOfMaybeUndefined => "W201",
            DiagnosticCode::W202_AssignmentToUndefined => "W202",

            // Type
            DiagnosticCode::E300_TypeMismatch => "E300",
            DiagnosticCode::E301_InvalidOperandType => "E301",
            DiagnosticCode::E302_InvalidArgumentType => "E302",
            DiagnosticCode::E303_InvalidReturnType => "E303",
            DiagnosticCode::E304_NotIndexable => "E304",

            // Semantic
            DiagnosticCode::E400_DuplicateDefinition => "E400",
            DiagnosticCode::E401_ArgumentCount => "E401",
            DiagnosticCode::E402_UnreachableCode => "E402",
            DiagnosticCode::E403_DivisionByZero => "E403",
            DiagnosticCode::E404_IntegerOverflow => "E404",

            // Linking
            DiagnosticCode::E500_UndefinedExternal => "E500",
            DiagnosticCode::E501_MissingEntryPoint => "E501",
            DiagnosticCode::E502_CyclicDependency => "E502",

            // Warnings
            DiagnosticCode::W001_UnusedVariable => "W001",
            DiagnosticCode::W002_UnusedFunction => "W002",
            DiagnosticCode::W003_UnreachableCode => "W003",
            DiagnosticCode::W004_ShadowedVariable => "W004",
            DiagnosticCode::W005_RedundantGuard => "W005",
            DiagnosticCode::W006_RedundantTypeCheck => "W006",
            DiagnosticCode::W007_ImplicitConversion => "W007",
            DiagnosticCode::W008_Deprecated => "W008",
            DiagnosticCode::W009_TypeMismatch => "W009",
        }
    }

    /// Get the default severity for this code
    pub fn severity(&self) -> Severity {
        match self {
            // Warnings
            DiagnosticCode::W001_UnusedVariable
            | DiagnosticCode::W002_UnusedFunction
            | DiagnosticCode::W003_UnreachableCode
            | DiagnosticCode::W004_ShadowedVariable
            | DiagnosticCode::W005_RedundantGuard
            | DiagnosticCode::W006_RedundantTypeCheck
            | DiagnosticCode::W007_ImplicitConversion
            | DiagnosticCode::W008_Deprecated
            | DiagnosticCode::W009_TypeMismatch => Severity::Warning,

            // Everything else is an error
            _ => Severity::Error,
        }
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

// ============================================================================
// Diagnostic Note
// ============================================================================

/// A related note attached to a diagnostic
#[derive(Debug, Clone)]
pub struct Note {
    /// Source file identity (when note spans a different file than the diagnostic)
    pub source_id: Option<Rc<str>>,
    /// Optional span for the note (may be None for general notes)
    pub span: Option<Span>,
    /// The note message
    pub message: String,
}

impl Note {
    /// Create a note with a span
    pub fn at(span: Span, message: impl Into<String>) -> Self {
        Note {
            source_id: None,
            span: Some(span),
            message: message.into(),
        }
    }

    /// Create a note without a span
    pub fn text(message: impl Into<String>) -> Self {
        Note {
            source_id: None,
            span: None,
            message: message.into(),
        }
    }
}

// ============================================================================
// Diagnostic
// ============================================================================

/// A single diagnostic message
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Severity level
    pub severity: Severity,
    /// Diagnostic code
    pub code: DiagnosticCode,
    /// Source file identity (canonical_id from SourceLoader)
    pub source_id: Option<Rc<str>>,
    /// Primary span (where the error occurred)
    pub span: Option<Span>,
    /// Primary message
    pub message: String,
    /// Related notes (additional context)
    pub notes: Vec<Note>,
}

impl Diagnostic {
    /// Create a new diagnostic
    pub fn new(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: code.severity(),
            code,
            source_id: None,
            span: None,
            message: message.into(),
            notes: Vec::new(),
        }
    }

    /// Create a diagnostic with a span
    pub fn at(code: DiagnosticCode, span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: code.severity(),
            code,
            source_id: None,
            span: Some(span),
            message: message.into(),
            notes: Vec::new(),
        }
    }

    /// Set the source file identity for this diagnostic
    pub fn in_source(&mut self, source_id: Rc<str>) -> &mut Self {
        self.source_id = Some(source_id);
        self
    }

    /// Add a note with a span
    pub fn note(&mut self, span: Span, message: impl Into<String>) -> &mut Self {
        self.notes.push(Note::at(span, message));
        self
    }

    /// Add a note without a span
    pub fn help(&mut self, message: impl Into<String>) -> &mut Self {
        self.notes.push(Note::text(message));
        self
    }

    /// Override the severity
    pub fn set_severity(&mut self, severity: Severity) -> &mut Self {
        self.severity = severity;
        self
    }

    /// Check if this is an error
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Check if this is a warning
    pub fn is_warning(&self) -> bool {
        self.severity == Severity::Warning
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]: {}", self.severity, self.code, self.message)?;
        if let Some(span) = &self.span {
            if let Some(source_id) = &self.source_id {
                write!(f, " (at {}:{}..{})", source_id, span.start, span.end)?;
            } else {
                write!(f, " (at {}..{})", span.start, span.end)?;
            }
        }
        for note in &self.notes {
            write!(f, "\n  note: {}", note.message)?;
            if let Some(span) = &note.span {
                if let Some(source_id) = &note.source_id {
                    write!(f, " (at {}:{}..{})", source_id, span.start, span.end)?;
                } else {
                    write!(f, " (at {}..{})", span.start, span.end)?;
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// Diagnostics Accumulator
// ============================================================================

/// Map from source file identity to source text.
///
/// Used by diagnostic rendering to convert byte offsets to line:column
/// positions and display source snippets. Populated during compilation
/// as each source file is loaded.
#[derive(Debug, Default, Clone)]
pub struct SourceMap {
    sources: HashMap<Rc<str>, String>,
}

impl SourceMap {
    /// Register a source file's text for diagnostic rendering.
    pub fn add(&mut self, source_id: Rc<str>, source: String) {
        self.sources.insert(source_id, source);
    }

    /// Look up the source text for a file.
    pub fn get(&self, source_id: &str) -> Option<&str> {
        self.sources.get(source_id).map(|s| s.as_str())
    }
}

/// Accumulator for diagnostics throughout compilation
///
/// Collects errors, warnings, and info messages without aborting on the first error.
/// This allows reporting multiple issues in a single compilation run.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
    /// Source text for each file, used for line:col rendering
    pub source_map: SourceMap,
    /// Current source file — automatically applied to new diagnostics
    current_source: Option<Rc<str>>,
}

impl Diagnostics {
    /// Create a new empty diagnostics accumulator
    pub fn new() -> Self {
        Diagnostics {
            items: Vec::new(),
            source_map: SourceMap::default(),
            current_source: None,
        }
    }

    /// Set the current source file. All subsequent diagnostics will be
    /// tagged with this source_id until changed or cleared.
    pub fn set_source(&mut self, source_id: Rc<str>) {
        self.current_source = Some(source_id);
    }

    /// Clear the current source file.
    pub fn clear_source(&mut self) {
        self.current_source = None;
    }

    /// Add a diagnostic (tagged with current source if set)
    pub fn emit(&mut self, mut diagnostic: Diagnostic) {
        if diagnostic.source_id.is_none() {
            diagnostic.source_id = self.current_source.clone();
        }
        self.items.push(diagnostic);
    }

    /// Emit an error with a span
    pub fn error(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let mut diag = Diagnostic::at(code, span, message);
        diag.source_id = self.current_source.clone();
        self.items.push(diag);
        self.items.last_mut().unwrap()
    }

    /// Emit an error without a span
    pub fn error_no_span(
        &mut self,
        code: DiagnosticCode,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let mut diag = Diagnostic::new(code, message);
        diag.source_id = self.current_source.clone();
        self.items.push(diag);
        self.items.last_mut().unwrap()
    }

    /// Emit a warning with a span
    pub fn warning(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let mut diag = Diagnostic::at(code, span, message);
        diag.severity = Severity::Warning;
        diag.source_id = self.current_source.clone();
        self.items.push(diag);
        self.items.last_mut().unwrap()
    }

    /// Emit a warning without a span
    pub fn warning_no_span(
        &mut self,
        code: DiagnosticCode,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let mut diag = Diagnostic::new(code, message);
        diag.severity = Severity::Warning;
        diag.source_id = self.current_source.clone();
        self.items.push(diag);
        self.items.last_mut().unwrap()
    }

    /// Emit an info message
    pub fn info(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let mut diag = Diagnostic::at(code, span, message);
        diag.severity = Severity::Info;
        diag.source_id = self.current_source.clone();
        self.items.push(diag);
        self.items.last_mut().unwrap()
    }

    /// Check if any errors have been emitted
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    /// Check if any warnings have been emitted
    pub fn has_warnings(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Warning)
    }

    /// Check if the accumulator is empty
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Get the number of diagnostics
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Get the number of errors
    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    /// Get the number of warnings
    pub fn warning_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }

    /// Get all diagnostics
    pub fn all(&self) -> &[Diagnostic] {
        &self.items
    }

    /// Get only errors
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| d.severity == Severity::Error)
    }

    /// Get only warnings
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Warning)
    }

    /// Iterate over all diagnostics
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    /// Clear all diagnostics
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Take all diagnostics, leaving the accumulator empty
    pub fn take(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.items)
    }

    /// Merge diagnostics from another accumulator
    pub fn merge(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
        for (id, text) in other.source_map.sources {
            self.source_map.sources.entry(id).or_insert(text);
        }
    }

    /// Convert to a Result, preserving warnings on success.
    ///
    /// - Ok: no errors — returns value and any warnings
    /// - Err: has errors — returns all diagnostics (errors + warnings)
    pub fn into_result<T>(self, value: T) -> Result<(T, Diagnostics), Diagnostics> {
        if self.has_errors() {
            Err(self)
        } else {
            Ok((value, self))
        }
    }

    /// Sort diagnostics by span (for consistent output)
    pub fn sort_by_span(&mut self) {
        self.items.sort_by(|a, b| {
            let a_start = a.span.map(|s| s.start).unwrap_or(0);
            let b_start = b.span.map(|s| s.start).unwrap_or(0);
            a_start.cmp(&b_start)
        });
    }

    /// Format a diagnostic's location as `file:line:col` or `line:col`.
    ///
    /// Uses the source map to convert byte offsets to line:column positions.
    /// Falls back to byte offsets if the source text is not in the map.
    pub fn format_location(&self, diag: &Diagnostic) -> Option<String> {
        let span = diag.span?;
        match &diag.source_id {
            Some(source_id) => {
                if let Some(source) = self.source_map.get(source_id) {
                    let lc = offset_to_line_col(source, span.start);
                    Some(format!("{}:{}:{}", source_id, lc.line, lc.col))
                } else {
                    Some(format!("{}:{}..{}", source_id, span.start, span.end))
                }
            }
            None => {
                // Single-file mode: no source_id, try the default source
                Some(format!("{}..{}", span.start, span.end))
            }
        }
    }

    /// Render a single diagnostic with source context.
    ///
    /// Produces rustc-style output:
    /// ```text
    /// error[E100]: undefined variable `foo`
    ///  --> utils.rill:12:5
    ///    |
    /// 12 |     foo + 1
    ///    |     ^^^ not found in this scope
    /// ```
    ///
    /// Falls back to a simple one-line format when source text is unavailable.
    pub fn render(&self, diag: &Diagnostic, out: &mut String) {
        use fmt::Write;

        // Header: severity[code]: message
        let _ = writeln!(out, "{}[{}]: {}", diag.severity, diag.code, diag.message);

        // Location + source context
        if let Some(span) = diag.span {
            let source_text = diag
                .source_id
                .as_ref()
                .and_then(|id| self.source_map.get(id));

            if let Some(source) = source_text {
                let start_lc = offset_to_line_col(source, span.start);
                let end_lc = offset_to_line_col(source, span.end);

                // --> file:line:col
                let file = diag
                    .source_id
                    .as_ref()
                    .map(|s| s.as_ref())
                    .unwrap_or("<input>");
                let _ = writeln!(out, " --> {}:{}:{}", file, start_lc.line, start_lc.col);

                // Source line with caret
                let (line_text, line_start) = source_line_at(source, span.start);
                let line_num = format!("{}", start_lc.line);
                let gutter = line_num.len();

                // Separator
                let _ = writeln!(out, "{:>gutter$} |", "");

                // Source line
                let _ = writeln!(out, "{} | {}", line_num, line_text);

                // Caret line — underline the span
                let col_start = span.start.saturating_sub(line_start);
                let col_end = if start_lc.line == end_lc.line {
                    span.end.saturating_sub(line_start)
                } else {
                    line_text.len() // span crosses lines — underline to end
                };
                let underline_len = col_end.saturating_sub(col_start).max(1);

                let _ = writeln!(
                    out,
                    "{:>gutter$} | {:>padding$}{}",
                    "",
                    "",
                    "^".repeat(underline_len),
                    padding = col_start,
                );
            } else {
                // No source text — show byte offsets
                if let Some(source_id) = &diag.source_id {
                    let _ = writeln!(out, " --> {}:{}..{}", source_id, span.start, span.end);
                } else {
                    let _ = writeln!(out, " --> {}..{}", span.start, span.end);
                }
            }
        }

        // Notes
        for note in &diag.notes {
            if let Some(span) = note.span {
                let source_text = note
                    .source_id
                    .as_ref()
                    .or(diag.source_id.as_ref())
                    .and_then(|id| self.source_map.get(id));

                if let Some(source) = source_text {
                    let lc = offset_to_line_col(source, span.start);
                    let file = note
                        .source_id
                        .as_ref()
                        .or(diag.source_id.as_ref())
                        .map(|s| s.as_ref())
                        .unwrap_or("<input>");
                    let _ = writeln!(
                        out,
                        " = note: {} ({}:{}:{})",
                        note.message, file, lc.line, lc.col
                    );
                } else {
                    let _ = writeln!(
                        out,
                        " = note: {} (at {}..{})",
                        note.message, span.start, span.end
                    );
                }
            } else {
                let _ = writeln!(out, " = help: {}", note.message);
            }
        }
    }

    /// Render all diagnostics with source context.
    pub fn render_all(&self) -> String {
        let mut out = String::new();
        for (i, diag) in self.items.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            self.render(diag, &mut out);
        }
        if self.has_errors() {
            use fmt::Write;
            let _ = writeln!(
                &mut out,
                "compilation failed: {} error(s), {} warning(s)",
                self.error_count(),
                self.warning_count()
            );
        }
        out
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render_all())
    }
}

// ============================================================================
// Source Location Utilities
// ============================================================================

/// A line:column location in source text (both 1-based)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for LineCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// Convert a byte offset to a line:column position in source text.
///
/// Both line and column are 1-based. If the offset is past the end of
/// the source, returns the position at the end.
///
/// Note: when multi-file support is added, the caller will need to
/// resolve which source file a span refers to before calling this.
pub fn offset_to_line_col(source: &str, offset: usize) -> LineCol {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut line_start = 0;

    for (i, ch) in source[..offset].char_indices() {
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }

    LineCol {
        line,
        col: offset - line_start + 1,
    }
}

/// Convert a span to start and end line:column positions.
pub fn span_to_line_col(source: &str, span: Span) -> (LineCol, LineCol) {
    (
        offset_to_line_col(source, span.start),
        offset_to_line_col(source, span.end),
    )
}

/// Get the source line containing a byte offset, and the byte offset of the line start.
fn source_line_at(source: &str, offset: usize) -> (&str, usize) {
    let offset = offset.min(source.len());
    let line_start = source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = source[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(source.len());
    (&source[line_start..line_end], line_start)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chumsky::span::Span;

    fn test_span(start: usize, end: usize) -> crate::ast::Span {
        crate::ast::Span::new((), start..end)
    }

    #[test]
    fn test_emit_error() {
        let mut diags = Diagnostics::new();

        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(10, 15),
            "undefined variable `foo`",
        );

        assert!(diags.has_errors());
        assert_eq!(diags.error_count(), 1);
        assert_eq!(diags.warning_count(), 0);
    }

    #[test]
    fn test_emit_warning() {
        let mut diags = Diagnostics::new();

        diags.warning(
            DiagnosticCode::W001_UnusedVariable,
            test_span(10, 15),
            "unused variable `bar`",
        );

        assert!(!diags.has_errors());
        assert!(diags.has_warnings());
        assert_eq!(diags.warning_count(), 1);
    }

    #[test]
    fn test_error_with_notes() {
        let mut diags = Diagnostics::new();

        diags
            .error(
                DiagnosticCode::E300_TypeMismatch,
                test_span(50, 60),
                "type mismatch: expected UInt, found Text",
            )
            .note(test_span(10, 20), "variable defined here as UInt")
            .help("consider using a type conversion");

        let diag = &diags.all()[0];
        assert_eq!(diag.notes.len(), 2);
    }

    #[test]
    fn test_multiple_diagnostics() {
        let mut diags = Diagnostics::new();

        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(10, 15),
            "undefined variable `x`",
        );

        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(30, 35),
            "undefined variable `y`",
        );

        diags.warning(
            DiagnosticCode::W001_UnusedVariable,
            test_span(50, 55),
            "unused variable `z`",
        );

        assert!(diags.has_errors());
        assert!(diags.has_warnings());
        assert_eq!(diags.error_count(), 2);
        assert_eq!(diags.warning_count(), 1);
        assert_eq!(diags.len(), 3);
    }

    #[test]
    fn test_into_result() {
        let diags = Diagnostics::new();
        let result = diags.into_result(42);
        assert!(result.is_ok());
        let (value, warnings) = result.unwrap();
        assert_eq!(value, 42);
        assert!(!warnings.has_warnings());

        let mut diags = Diagnostics::new();
        diags.error_no_span(DiagnosticCode::E100_UndefinedVariable, "error");
        let result = diags.into_result(42);
        assert!(result.is_err());
    }

    #[test]
    fn test_into_result_preserves_warnings() {
        let mut diags = Diagnostics::new();
        diags.warning_no_span(DiagnosticCode::W001_UnusedVariable, "unused x");
        let result = diags.into_result(42);
        assert!(result.is_ok());
        let (value, warnings) = result.unwrap();
        assert_eq!(value, 42);
        assert!(warnings.has_warnings());
        assert_eq!(warnings.warning_count(), 1);
    }

    #[test]
    fn test_diagnostic_display() {
        let mut diag = Diagnostic::at(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(10, 15),
            "undefined variable `x`",
        );
        diag.help("did you mean `y`?");

        let s = diag.to_string();
        assert!(s.contains("E100"));
        assert!(s.contains("undefined variable"));
        assert!(s.contains("did you mean"));
    }

    #[test]
    fn test_sort_by_span() {
        let mut diags = Diagnostics::new();

        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(50, 55),
            "error at 50",
        );
        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(10, 15),
            "error at 10",
        );
        diags.error(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(30, 35),
            "error at 30",
        );

        diags.sort_by_span();

        let spans: Vec<_> = diags.iter().map(|d| d.span.unwrap().start).collect();
        assert_eq!(spans, vec![10, 30, 50]);
    }

    // ========================================================================
    // Source Location Tests
    // ========================================================================

    #[test]
    fn test_offset_to_line_col_single_line() {
        let src = "let x = 42;";
        assert_eq!(offset_to_line_col(src, 0), LineCol { line: 1, col: 1 });
        assert_eq!(offset_to_line_col(src, 4), LineCol { line: 1, col: 5 });
        assert_eq!(offset_to_line_col(src, 10), LineCol { line: 1, col: 11 });
    }

    #[test]
    fn test_offset_to_line_col_multi_line() {
        let src = "fn test() {\n    let x = 1;\n    return x;\n}";
        // "fn test() {\n" = 12 chars, line 1
        // "    let x = 1;\n" = 14 chars, line 2
        // "    return x;\n" = 14 chars, line 3
        // "}" = 1 char, line 4
        assert_eq!(offset_to_line_col(src, 0), LineCol { line: 1, col: 1 });
        assert_eq!(offset_to_line_col(src, 11), LineCol { line: 1, col: 12 }); // '{'
        assert_eq!(offset_to_line_col(src, 12), LineCol { line: 2, col: 1 }); // after \n
        assert_eq!(offset_to_line_col(src, 16), LineCol { line: 2, col: 5 }); // 'l' in let
    }

    #[test]
    fn test_offset_to_line_col_past_end() {
        let src = "hi";
        assert_eq!(offset_to_line_col(src, 100), LineCol { line: 1, col: 3 });
    }

    #[test]
    fn test_span_to_line_col() {
        let src = "let x = 42;\nlet y = x + 1;";
        let span = test_span(16, 17); // 'x' on line 2
        let (start, end) = span_to_line_col(src, span);
        assert_eq!(start, LineCol { line: 2, col: 5 });
        assert_eq!(end, LineCol { line: 2, col: 6 });
    }

    #[test]
    fn test_line_col_display() {
        let lc = LineCol { line: 3, col: 12 };
        assert_eq!(lc.to_string(), "3:12");
    }

    // ========================================================================
    // Pretty Rendering Tests
    // ========================================================================

    #[test]
    fn test_render_with_source_context() {
        let source = "fn test() {\n    foo + 1;\n}";
        let mut diags = Diagnostics::new();
        diags
            .source_map
            .add(Rc::from("test.rill"), source.to_string());

        let mut d = Diagnostic::at(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(16, 19), // "foo" on line 2
            "undefined variable `foo`",
        );
        d.in_source(Rc::from("test.rill"));
        diags.emit(d);

        let rendered = diags.render_all();
        assert!(rendered.contains("error[E100]: undefined variable `foo`"));
        assert!(rendered.contains("--> test.rill:2:5"));
        assert!(rendered.contains("    foo + 1;"));
        assert!(rendered.contains("^^^"));
    }

    #[test]
    fn test_render_with_note() {
        let source = "fn test() {\n    let x = 1;\n    let x = 2;\n}";
        let mut diags = Diagnostics::new();
        diags
            .source_map
            .add(Rc::from("test.rill"), source.to_string());

        let mut d = Diagnostic::at(
            DiagnosticCode::E400_DuplicateDefinition,
            test_span(31, 32), // second "x" on line 3
            "duplicate variable `x`",
        );
        d.in_source(Rc::from("test.rill"));
        d.note(test_span(16, 17), "previously defined here");
        diags.emit(d);

        let rendered = diags.render_all();
        assert!(rendered.contains("error[E400]"));
        assert!(rendered.contains("--> test.rill:3:"));
        assert!(rendered.contains("note: previously defined here"));
    }

    #[test]
    fn test_render_without_source() {
        let mut diags = Diagnostics::new();
        diags.error_no_span(
            DiagnosticCode::E500_UndefinedExternal,
            "failed to load 'missing.rill'",
        );

        let rendered = diags.render_all();
        assert!(rendered.contains("error[E500]: failed to load"));
        // No source context — just the message
        assert!(!rendered.contains("-->"));
    }

    #[test]
    fn test_render_single_char_span() {
        let source = "x";
        let mut diags = Diagnostics::new();
        diags
            .source_map
            .add(Rc::from("test.rill"), source.to_string());

        let mut d = Diagnostic::at(
            DiagnosticCode::E100_UndefinedVariable,
            test_span(0, 1),
            "undefined variable `x`",
        );
        d.in_source(Rc::from("test.rill"));
        diags.emit(d);

        let rendered = diags.render_all();
        assert!(rendered.contains("--> test.rill:1:1"));
        assert!(rendered.contains("^"));
    }
}
