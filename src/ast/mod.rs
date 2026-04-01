//! Abstract Syntax Tree and Parser
//!
//! This module contains:
//! - AST type definitions (expressions, statements, patterns, programs)
//! - Chumsky-based parser that produces the AST

mod types;

pub(crate) mod parser;

// Re-export all AST types
pub use types::*;
