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

// Lowering submodules
mod constant;
mod control;
mod expr;
mod pattern;
mod program;
mod stmt;

// Re-export all IR types
pub use types::*;

// Parent module imports
use super::*;
use chumsky::span::Span;
use std::collections::HashMap;

/// Create a placeholder span (for errors where we don't have source location)
pub(crate) fn dummy_span() -> ast::Span {
    ast::Span::new((), 0..0)
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during lowering
#[derive(Debug, Clone)]
pub enum LowerError {
    /// Reference to undefined variable
    UndefinedVariable { name: String, span: ast::Span },

    /// Type error detected during lowering
    TypeError { message: String, span: ast::Span },

    /// Invalid pattern in context
    InvalidPattern { message: String, span: ast::Span },

    /// Break/continue outside of loop
    InvalidLoopControl { kind: &'static str, span: ast::Span },

    /// Other semantic errors
    SemanticError { message: String, span: ast::Span },
}

pub type Result<T> = std::result::Result<T, LowerError>;

// ============================================================================
// Binding Mode
// ============================================================================

/// Binding mode for pattern matching
#[derive(Clone, Copy)]
pub(crate) enum BindingMode {
    /// let - by value (copy)
    Value,
    /// with - by reference
    Reference,
}

// ============================================================================
// Lowerer State
// ============================================================================

/// Main lowering context
pub struct Lowerer<'a> {
    /// Registry of builtin functions (for const evaluation)
    pub(crate) builtins: &'a builtins::BuiltinRegistry,

    /// Evaluated constant values (for referencing in other constants)
    pub(crate) const_bindings: HashMap<String, ConstValue>,

    // ID generation
    pub(crate) next_var_id: u32,
    pub(crate) next_block_id: u32,

    /// Stack of scopes for variable name resolution
    pub(crate) scopes: Vec<HashMap<String, VarId>>,

    /// All variables declared in the current function
    pub(crate) vars: Vec<Var>,

    /// All basic blocks in the current function
    pub(crate) blocks: Vec<BasicBlock>,

    /// The block currently being built
    pub(crate) current_block: BlockId,

    /// Instructions accumulated for the current block
    pub(crate) current_instructions: Vec<Instruction>,

    /// Stack of (break_target, continue_target) for nested loops
    pub(crate) loop_stack: Vec<LoopContext>,
}

/// Context for a loop (for break/continue)
pub(crate) struct LoopContext {
    pub(crate) break_target: BlockId,
    pub(crate) continue_target: BlockId,
    pub(crate) break_values: Vec<(BlockId, VarId)>,
}

impl<'a> Lowerer<'a> {
    /// Create a new lowerer with the given builtin registry
    pub fn new(builtins: &'a builtins::BuiltinRegistry) -> Self {
        Lowerer {
            builtins,
            const_bindings: HashMap::new(),
            next_var_id: 0,
            next_block_id: 0,
            scopes: Vec::new(),
            vars: Vec::new(),
            blocks: Vec::new(),
            current_block: BlockId(0),
            current_instructions: Vec::new(),
            loop_stack: Vec::new(),
        }
    }

    // ========================================================================
    // ID Generation
    // ========================================================================

    pub(crate) fn fresh_var(&mut self) -> VarId {
        let id = VarId(self.next_var_id);
        self.next_var_id += 1;
        id
    }

    pub(crate) fn fresh_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        id
    }

    // ========================================================================
    // Variable Management
    // ========================================================================

    pub(crate) fn new_var(&mut self, name: ast::Identifier, type_set: TypeSet) -> VarId {
        let id = self.fresh_var();
        self.vars.push(Var::new(id, name, type_set));
        id
    }

    pub(crate) fn new_temp(&mut self, type_set: TypeSet) -> VarId {
        self.new_var(ast::Identifier("$tmp".to_string()), type_set)
    }

    // ========================================================================
    // Scope Management
    // ========================================================================

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn bind(&mut self, name: &str, var: VarId) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), var);
        }
    }

    pub(crate) fn lookup(&self, name: &str) -> Option<VarId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&var) = scope.get(name) {
                return Some(var);
            }
        }
        None
    }

    // ========================================================================
    // Block Management
    // ========================================================================

    pub(crate) fn start_block(&mut self) -> BlockId {
        let id = self.fresh_block();
        self.current_block = id;
        self.current_instructions = Vec::new();
        id
    }

    pub(crate) fn finish_block(&mut self, terminator: Terminator) {
        let block = BasicBlock {
            id: self.current_block,
            instructions: std::mem::take(&mut self.current_instructions),
            terminator,
        };
        self.blocks.push(block);
    }

    pub(crate) fn emit(&mut self, instruction: Instruction) {
        self.current_instructions.push(instruction);
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// All base types (for unknown/any type)
pub(crate) fn all_types() -> impl Iterator<Item = types::BaseType> {
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
    ]
    .into_iter()
}

// ============================================================================
// Public API
// ============================================================================

/// Lower an AST program to IR with the given builtin registry
pub fn lower(program: &ast::Program, builtins: &builtins::BuiltinRegistry) -> Result<Program> {
    let mut lowerer = Lowerer::new(builtins);
    lowerer.lower_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> builtins::BuiltinRegistry {
        let mut registry = builtins::BuiltinRegistry::new();
        builtins::register_core_builtins(&mut registry);
        registry
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
        let ast = parser::parse(source).expect("parse failed");
        let ir = lower(&ast, &registry).expect("lower failed");

        assert_eq!(ir.functions.len(), 1);
        assert_eq!(ir.functions[0].name.0, "test");
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
        let ast = parser::parse(source).expect("parse failed");
        let ir = lower(&ast, &registry).expect("lower failed");

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
        let ast = parser::parse(source).expect("parse failed");
        let ir = lower(&ast, &registry).expect("lower failed");

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
        let ast = parser::parse(source).expect("parse failed");
        let ir = lower(&ast, &registry).expect("lower failed");

        assert_eq!(ir.constants.len(), 2);
        assert_eq!(ir.constants[0].name.0, "MAX_TTL");
        assert_eq!(ir.constants[0].value, ConstValue::UInt(86400));
        assert_eq!(ir.constants[1].name.0, "DOUBLE");
        assert_eq!(ir.constants[1].value, ConstValue::UInt(172800));
    }

    #[test]
    fn test_lower_constant_array_destructure() {
        let source = r#"
            const [A, B, C] = [1, 2, 3];
            fn test() { }
        "#;

        let registry = test_registry();
        let ast = parser::parse(source).expect("parse failed");
        let ir = lower(&ast, &registry).expect("lower failed");

        assert_eq!(ir.constants.len(), 3);
        assert_eq!(ir.constants[0].name.0, "A");
        assert_eq!(ir.constants[0].value, ConstValue::UInt(1));
        assert_eq!(ir.constants[1].name.0, "B");
        assert_eq!(ir.constants[1].value, ConstValue::UInt(2));
        assert_eq!(ir.constants[2].name.0, "C");
        assert_eq!(ir.constants[2].value, ConstValue::UInt(3));
    }
}
