mod ast;
mod compile;
pub mod diagnostics;
pub mod exec;
pub mod externs;
mod ir;
mod parser;
pub mod types;

// Re-export key types for convenient access
pub use diagnostics::{Diagnostics, LineCol, offset_to_line_col, span_to_line_col};
pub use exec::{ExecError, VM, Value};
pub use externs::ExternRegistry;
pub use types::{BaseType, TypeSet};

/// Compiled Rill program, ready for execution.
///
/// This is an opaque handle produced by [`compile()`]. The internal
/// representation may change between versions — do not depend on its
/// structure. A future serialization format will allow saving/loading
/// compiled programs without re-compilation.
pub struct Program {
    compiled: compile::CompiledProgram,
}

impl Program {
    /// Resolve a function by name, returning a handle for repeated calls.
    ///
    /// Performs the name lookup once. The returned [`Function`] can be called
    /// many times without further lookup overhead — critical for hot-path
    /// embedding where the same program processes many inputs.
    ///
    /// ```ignore
    /// let process = program.function("process").expect("function exists");
    /// for input in inputs {
    ///     vm.push(input)?;
    ///     let result = process.call(&mut vm, 1)?;
    /// }
    /// ```
    pub fn function(&self, name: &str) -> Option<FunctionHandle<'_>> {
        self.compiled
            .func_index
            .get(name)
            .map(|&idx| FunctionHandle {
                program: &self.compiled,
                func_idx: idx,
            })
    }

    /// Call a named function (convenience method — does a name lookup each time).
    ///
    /// Push arguments onto the VM stack before calling:
    /// ```ignore
    /// vm.push(Value::UInt(42))?;
    /// vm.push(Value::Text("hello".into()))?;
    /// let result = program.call(&mut vm, "process", 2)?;
    /// ```
    ///
    /// For repeated calls to the same function, use [`function()`] to resolve
    /// the name once and then call the returned handle.
    pub fn call(
        &self,
        vm: &mut VM,
        func_name: &str,
        argc: usize,
    ) -> Result<Option<Value>, ExecError> {
        compile::execute(&self.compiled, vm, func_name, argc)
    }
}

/// A resolved function handle — no name lookup on each call.
///
/// Obtained from [`Program::function()`]. Holds a reference to the program
/// and the resolved function index. Use this for hot-path execution where
/// the same function is called repeatedly with different data.
pub struct FunctionHandle<'a> {
    program: &'a compile::CompiledProgram,
    func_idx: usize,
}

impl<'a> FunctionHandle<'a> {
    /// Execute this function with the given arguments.
    ///
    /// Push arguments onto the VM stack before calling:
    /// ```ignore
    /// vm.push(value)?;
    /// let result = handle.call(&mut vm, 1)?;
    /// ```
    pub fn call(&self, vm: &mut VM, argc: usize) -> Result<Option<Value>, ExecError> {
        compile::execute_by_index(self.program, vm, self.func_idx, argc)
    }
}

/// Compile source code into an executable program.
///
/// Runs the full pipeline: parse → lower → optimize → compile to closures.
///
/// Returns `Ok((program, diagnostics))` on success (diagnostics may contain
/// warnings), or `Err(diagnostics)` if there were compilation errors.
pub fn compile(
    source: &str,
    externs: &ExternRegistry,
) -> Result<(Program, Diagnostics), Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    let ast = match parser::parse(source, &mut diagnostics) {
        Some(ast) => ast,
        None => return Err(diagnostics),
    };

    let mut ir_program = match ir::lower(&ast, externs, &mut diagnostics) {
        Some(ir) => ir,
        None => return Err(diagnostics),
    };

    ir::opt::optimize(&mut ir_program, externs, &mut diagnostics);

    // Compile IR to closure-threaded code (includes link phase)
    let mut compiled = match compile::compile_program(&ir_program, externs) {
        Ok(compiled) => compiled,
        Err(link_errors) => {
            diagnostics.merge(link_errors);
            return Err(diagnostics);
        }
    };

    // Merge any link-phase warnings (unused functions, etc.)
    diagnostics.merge(std::mem::take(&mut compiled.warnings));

    Ok((Program { compiled }, diagnostics))
}

/// Create an extern registry with standard externs registered.
pub fn standard_externs() -> ExternRegistry {
    externs::standard_externs()
}
