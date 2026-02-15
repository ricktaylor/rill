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
use std::fmt;

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
    // Definedness Analysis Errors (E200-E299)
    // ========================================================================
    /// Use of definitely undefined value
    E200_UseOfUndefined,
    /// Use of possibly undefined value without guard
    E201_UseOfMaybeUndefined,
    /// Assignment to undefined location
    E202_AssignmentToUndefined,

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
            DiagnosticCode::E200_UseOfUndefined => "E200",
            DiagnosticCode::E201_UseOfMaybeUndefined => "E201",
            DiagnosticCode::E202_AssignmentToUndefined => "E202",

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
            | DiagnosticCode::W008_Deprecated => Severity::Warning,

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
    /// Optional span for the note (may be None for general notes)
    pub span: Option<Span>,
    /// The note message
    pub message: String,
}

impl Note {
    /// Create a note with a span
    pub fn at(span: Span, message: impl Into<String>) -> Self {
        Note {
            span: Some(span),
            message: message.into(),
        }
    }

    /// Create a note without a span
    pub fn text(message: impl Into<String>) -> Self {
        Note {
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
            span: Some(span),
            message: message.into(),
            notes: Vec::new(),
        }
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
            write!(f, " (at {}..{})", span.start, span.end)?;
        }
        for note in &self.notes {
            write!(f, "\n  note: {}", note.message)?;
            if let Some(span) = &note.span {
                write!(f, " (at {}..{})", span.start, span.end)?;
            }
        }
        Ok(())
    }
}

// ============================================================================
// Diagnostics Accumulator
// ============================================================================

/// Accumulator for diagnostics throughout compilation
///
/// Collects errors, warnings, and info messages without aborting on the first error.
/// This allows reporting multiple issues in a single compilation run.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    /// Create a new empty diagnostics accumulator
    pub fn new() -> Self {
        Diagnostics { items: Vec::new() }
    }

    /// Add a diagnostic
    pub fn emit(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    /// Emit an error with a span
    pub fn error(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.items.push(Diagnostic::at(code, span, message));
        self.items.last_mut().unwrap()
    }

    /// Emit an error without a span
    pub fn error_no_span(
        &mut self,
        code: DiagnosticCode,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.items.push(Diagnostic::new(code, message));
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
    }

    /// Convert to a Result - Ok if no errors, Err with all diagnostics if any errors
    pub fn into_result<T>(self, value: T) -> Result<T, Diagnostics> {
        if self.has_errors() {
            Err(self)
        } else {
            Ok(value)
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
        for (i, diag) in self.items.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{}", diag)?;
        }
        if self.has_errors() {
            writeln!(f)?;
            write!(
                f,
                "compilation failed: {} error(s), {} warning(s)",
                self.error_count(),
                self.warning_count()
            )?;
        }
        Ok(())
    }
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
        let mut diags = Diagnostics::new();
        let result = diags.into_result(42);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);

        let mut diags = Diagnostics::new();
        diags.error_no_span(DiagnosticCode::E100_UndefinedVariable, "error");
        let result = diags.into_result(42);
        assert!(result.is_err());
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
}
