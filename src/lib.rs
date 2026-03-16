mod ast;
pub mod builtins;
pub mod diagnostics;
pub mod exec;
mod ir;
mod parser;
pub mod types;

// Re-export key types for convenient access
pub use builtins::BuiltinRegistry;
pub use diagnostics::{Diagnostics, LineCol, offset_to_line_col, span_to_line_col};
pub use exec::{ExecError, VM, Value};
pub use types::{BaseType, TypeSet};

/// Compiled Rill program, ready for execution.
///
/// This is an opaque handle produced by [`compile()`]. The internal
/// representation may change between versions — do not depend on its
/// structure. A future serialization format will allow saving/loading
/// compiled programs without re-compilation.
pub struct Program {
    ir: ir::IrProgram,
}

/// Compile source code into an executable program.
///
/// Runs the full pipeline: parse → lower → optimize.
///
/// Returns `Ok((program, diagnostics))` on success (diagnostics may contain
/// warnings), or `Err(diagnostics)` if there were compilation errors.
pub fn compile(
    source: &str,
    builtins: &BuiltinRegistry,
) -> Result<(Program, Diagnostics), Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    let ast = match parser::parse(source, &mut diagnostics) {
        Some(ast) => ast,
        None => return Err(diagnostics),
    };

    let mut ir_program = match ir::lower(&ast, builtins, &mut diagnostics) {
        Some(ir) => ir,
        None => return Err(diagnostics),
    };

    ir::opt::optimize(&mut ir_program, builtins, &mut diagnostics);

    Ok((Program { ir: ir_program }, diagnostics))
}

/// Create a builtin registry with all standard builtins registered.
pub fn standard_builtins() -> BuiltinRegistry {
    builtins::standard_builtins()
}
