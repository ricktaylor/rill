//! Intermediate Representation and Lowering
//!
//! This module contains:
//! - IR type definitions (SSA form with type set tracking)
//! - AST to IR lowering
//!
//! # Lowering Design
//!
//! The lowering process:
//! 1. Each function is lowered independently (no closures/captures)
//! 2. Expressions produce VarIds (the SSA variable holding the result)
//! 3. Statements emit instructions and may modify scope
//! 4. Patterns are decomposed into control flow (Match, Guard, If terminators)
//!
//! # Scope Management
//!
//! Uses a Vec of HashMaps for lexical scoping:
//! - Push on block entry (if, for, match arms, etc.)
//! - Pop on block exit
//! - Lookup walks backwards to find bindings

// IR type definitions
mod types;

// Shared constant evaluation utilities
pub mod const_eval;

// Lowering submodules
mod constant;
mod control;
mod expr;
mod pattern;
mod program;
mod stmt;

// Optimization passes
pub mod opt;

// Re-export all IR types
pub use types::*;

// Parent module imports
use super::*;
use diagnostics::{DiagnosticCode, Diagnostics};
use std::collections::HashMap;

// ============================================================================
// Binding Mode
// ============================================================================

/// Binding mode for pattern matching
#[derive(Clone, Copy)]
pub enum BindingMode {
    /// let - by value (copy)
    Value,
    /// with - by reference
    Reference,
}

/// Origin of a reference binding: tracks what a `with`-bound variable refers to.
///
/// Used by the lowerer to emit `WriteRef` instructions when a ref-backed
/// variable is assigned. The optimizer can then see write-back explicitly.
#[derive(Clone)]
pub struct RefOrigin {
    /// The MakeRef dest VarId (the reference variable)
    pub ref_var: VarId,
    /// The collection or variable being referenced
    pub base: VarId,
    /// The key into the collection (None for whole-value refs)
    pub key: Option<VarId>,
}

// ============================================================================
// Lowerer State
// ============================================================================

/// Main lowering context
pub struct Lowerer<'a> {
    /// Registry of builtin functions (for const evaluation)
    pub builtins: &'a builtins::BuiltinRegistry,

    /// Diagnostics accumulator for errors and warnings
    pub diagnostics: &'a mut Diagnostics,

    /// Evaluated constant values (for referencing in other constants)
    pub const_bindings: HashMap<ast::Identifier, ConstValue>,

    // ID generation
    pub next_var_id: u32,
    pub next_block_id: u32,

    /// Stack of scopes for variable name resolution
    pub scopes: Vec<HashMap<ast::Identifier, VarId>>,

    /// Reference origin tracking (scoped like `scopes`).
    /// Maps variable names to their RefOrigin when bound via `with`.
    pub ref_origins: Vec<HashMap<ast::Identifier, RefOrigin>>,

    /// All variables declared in the current function
    pub vars: Vec<Var>,

    /// All basic blocks in the current function
    pub blocks: Vec<BasicBlock>,

    /// The block currently being built
    pub current_block: BlockId,

    /// Instructions accumulated for the current block
    pub current_instructions: Vec<SpannedInst>,

    /// Current source span (for instruction provenance)
    pub current_span: ast::Span,

    /// Stack of (break_target, continue_target) for nested loops
    pub loop_stack: Vec<LoopContext>,
}

/// Context for a loop (for break/continue)
pub struct LoopContext {
    pub break_target: BlockId,
    pub continue_target: BlockId,
    pub break_values: Vec<(BlockId, VarId)>,
}

impl<'a> Lowerer<'a> {
    /// Create a new lowerer with the given builtin registry and diagnostics
    pub fn new(builtins: &'a builtins::BuiltinRegistry, diagnostics: &'a mut Diagnostics) -> Self {
        Lowerer {
            builtins,
            diagnostics,
            const_bindings: HashMap::new(),
            next_var_id: 0,
            next_block_id: 0,
            scopes: Vec::new(),
            ref_origins: Vec::new(),
            vars: Vec::new(),
            blocks: Vec::new(),
            current_block: BlockId(0),
            current_instructions: Vec::new(),
            current_span: ast::Span::default(),
            loop_stack: Vec::new(),
        }
    }

    // ========================================================================
    // Error Emission
    // ========================================================================

    /// Emit an error for an undefined variable
    pub fn error_undefined_var(&mut self, namespace: Option<&str>, name: &str, span: ast::Span) {
        let msg = match namespace {
            Some(ns) => format!("undefined variable `{}::{}`", ns, name),
            None => format!("undefined variable `{}`", name),
        };
        self.diagnostics
            .error(DiagnosticCode::E100_UndefinedVariable, span, msg);
    }

    /// Emit an error for invalid loop control (break/continue outside loop)
    pub fn error_invalid_loop_control(&mut self, kind: &str, span: ast::Span) {
        self.diagnostics.error(
            DiagnosticCode::E103_InvalidLoopControl,
            span,
            format!("`{}` outside of loop", kind),
        );
    }

    /// Emit an error for an invalid pattern
    pub fn error_invalid_pattern(&mut self, message: &str, span: ast::Span) {
        self.diagnostics
            .error(DiagnosticCode::E105_InvalidPattern, span, message);
    }

    /// Emit an error for failed constant evaluation
    pub fn error_const_eval(&mut self, message: &str, span: ast::Span) {
        self.diagnostics
            .error(DiagnosticCode::E106_ConstEvalFailed, span, message);
    }

    /// Create an undefined value as error recovery placeholder
    pub fn error_placeholder(&mut self) -> VarId {
        let dest = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest });
        dest
    }

    // ========================================================================
    // ID Generation
    // ========================================================================

    pub fn fresh_var(&mut self) -> VarId {
        let id = VarId(self.next_var_id);
        self.next_var_id += 1;
        id
    }

    pub fn fresh_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        id
    }

    // ========================================================================
    // Variable Management
    // ========================================================================

    pub fn new_var(&mut self, name: ast::Identifier, type_set: TypeSet) -> VarId {
        let id = self.fresh_var();
        self.vars.push(Var::new(id, name, type_set));
        id
    }

    pub fn new_temp(&mut self, type_set: TypeSet) -> VarId {
        self.new_var(ast::Identifier("$tmp".to_string()), type_set)
    }

    // ========================================================================
    // Scope Management
    // ========================================================================

    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.ref_origins.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
        self.ref_origins.pop();
    }

    pub fn bind(&mut self, name: &ast::Identifier, var: VarId) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), var);
        }
    }

    pub fn lookup(&self, name: &ast::Identifier) -> Option<VarId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&var) = scope.get(name) {
                return Some(var);
            }
        }
        None
    }

    // ========================================================================
    // Reference Origin Tracking
    // ========================================================================

    /// Record that `name` is a reference binding with the given origin.
    pub fn bind_ref(&mut self, name: &ast::Identifier, origin: RefOrigin) {
        if let Some(scope) = self.ref_origins.last_mut() {
            scope.insert(name.clone(), origin);
        }
    }

    /// Look up whether `name` is a reference-backed variable.
    pub fn lookup_ref(&self, name: &ast::Identifier) -> Option<&RefOrigin> {
        for scope in self.ref_origins.iter().rev() {
            if let Some(origin) = scope.get(name) {
                return Some(origin);
            }
        }
        None
    }

    // ========================================================================
    // Block Management
    // ========================================================================

    pub fn start_block(&mut self) -> BlockId {
        let id = self.fresh_block();
        self.current_block = id;
        self.current_instructions = Vec::new();
        id
    }

    pub fn finish_block(&mut self, terminator: Terminator) {
        let block = BasicBlock {
            id: self.current_block,
            instructions: std::mem::take(&mut self.current_instructions),
            terminator,
        };
        self.blocks.push(block);
    }

    pub fn emit(&mut self, instruction: Instruction) {
        self.current_instructions
            .push(ast::Spanned::new(instruction, self.current_span));
    }

    /// Set the current span for subsequent instructions
    pub fn set_span(&mut self, span: ast::Span) {
        self.current_span = span;
    }

    /// Lower a spanned statement, setting the current span first
    pub fn lower_stmt(&mut self, stmt: &ast::Stmt) {
        self.set_span(stmt.span);
        self.lower_statement(&stmt.node);
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// All base types (for unknown/any type)
pub fn all_types() -> impl Iterator<Item = types::BaseType> {
    use types::BaseType;
    [
        BaseType::Bool,
        BaseType::UInt,
        BaseType::Int,
        BaseType::Float,
        BaseType::Text,
        BaseType::Bytes,
        BaseType::Array,
        BaseType::Map,
        BaseType::Sequence,
    ]
    .into_iter()
}

// ============================================================================
// Public API
// ============================================================================

/// Lower an AST program to IR with the given builtin registry
///
/// Errors are emitted to the diagnostics accumulator. Returns `Some(IrProgram)` if
/// lowering succeeded (possibly with warnings), `None` if there were errors.
pub fn lower(
    program: &ast::AstProgram,
    builtins: &builtins::BuiltinRegistry,
    diagnostics: &mut Diagnostics,
) -> Option<IrProgram> {
    let mut lowerer = Lowerer::new(builtins, diagnostics);
    lowerer.lower_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Diagnostics;

    fn test_registry() -> builtins::BuiltinRegistry {
        builtins::standard_builtins()
    }

    fn try_parse(source: &str) -> ast::AstProgram {
        let mut diags = Diagnostics::new();
        parser::parse(source, &mut diags).expect("parse failed")
    }

    fn try_lower(ast: &ast::AstProgram, registry: &builtins::BuiltinRegistry) -> IrProgram {
        let mut diags = Diagnostics::new();
        lower(ast, registry, &mut diags).expect("lower failed")
    }

    #[test]
    fn test_lower_simple_function() {
        let source = r#"
            fn test(x) {
                let y = x + 1;
                return y;
            }
        "#;

        let registry = test_registry();
        let ast = try_parse(source);
        let ir = try_lower(&ast, &registry);

        assert_eq!(ir.functions.len(), 1);
        assert_eq!(ir.functions[0].name.as_ref(), "test");
        assert_eq!(ir.functions[0].params.len(), 1);
    }

    #[test]
    fn test_lower_if_expression() {
        let source = r#"
            fn test(x) {
                if x { 1 } else { 2 }
            }
        "#;

        let registry = test_registry();
        let ast = try_parse(source);
        let ir = try_lower(&ast, &registry);

        assert!(ir.functions[0].blocks.len() >= 4);
    }

    #[test]
    fn test_lower_while_loop() {
        let source = r#"
            fn test(x) {
                while x {
                    x = false;
                }
            }
        "#;

        let registry = test_registry();
        let ast = try_parse(source);
        let ir = try_lower(&ast, &registry);

        assert!(ir.functions[0].blocks.len() >= 3);
    }

    #[test]
    fn test_lower_constant() {
        let source = r#"
            const MAX_TTL = 86400;
            const DOUBLE = MAX_TTL * 2;
            fn test() { }
        "#;

        let registry = test_registry();
        let ast = try_parse(source);
        let ir = try_lower(&ast, &registry);

        assert_eq!(ir.constants.len(), 2);
        assert_eq!(ir.constants[0].name.as_ref(), "MAX_TTL");
        assert_eq!(ir.constants[0].value, ConstValue::UInt(86400));
        assert_eq!(ir.constants[1].name.as_ref(), "DOUBLE");
        assert_eq!(ir.constants[1].value, ConstValue::UInt(172800));
    }

    #[test]
    fn test_lower_constant_array_destructure() {
        let source = r#"
            const [A, B, C] = [1, 2, 3];
            fn test() { }
        "#;

        let registry = test_registry();
        let ast = try_parse(source);
        let ir = try_lower(&ast, &registry);

        assert_eq!(ir.constants.len(), 3);
        assert_eq!(ir.constants[0].name.as_ref(), "A");
        assert_eq!(ir.constants[0].value, ConstValue::UInt(1));
        assert_eq!(ir.constants[1].name.as_ref(), "B");
        assert_eq!(ir.constants[1].value, ConstValue::UInt(2));
        assert_eq!(ir.constants[2].name.as_ref(), "C");
        assert_eq!(ir.constants[2].value, ConstValue::UInt(3));
    }
}
