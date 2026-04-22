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
/// variable is assigned. The `ref_var` is the dest of the MakeRef instruction;
/// the compiler resolves it to (base, key) via `build_ref_map` at compile time.
/// `base_name` is the named variable being mutated (for SSA Reload after WriteRef).
#[derive(Clone)]
pub struct RefOrigin {
    /// The MakeRef dest VarId (the reference variable)
    pub ref_var: VarId,
    /// The MakeRef base VarId (the collection being referenced)
    pub base_var: VarId,
    /// The named variable holding the base (for Reload after WriteRef)
    pub base_name: Option<ast::Identifier>,
    /// True if this is a whole-value ref (MakeRef key: None).
    /// Whole-value refs can change the base's type entirely on write,
    /// so WriteRef needs Reload. Element refs (key: Some) only mutate
    /// collection contents — container type is unchanged.
    pub whole_value: bool,
}

// ============================================================================
// Lowerer State
// ============================================================================

/// Main lowering context
pub struct Lowerer<'a> {
    /// Registry of extern functions (for const evaluation)
    pub externs: &'a externs::ExternRegistry,

    /// Diagnostics accumulator for errors and warnings
    pub diagnostics: &'a mut Diagnostics,

    /// Evaluated constant values (for referencing in other constants)
    pub const_bindings: HashMap<ast::Identifier, ConstValue>,

    // ID generation
    pub next_var_id: u32,
    pub next_block_id: u32,
    pub next_slot_id: u32,

    /// Stack of scopes for variable name resolution.
    ///
    /// Maps variable names to slot IDs. Each binding site (`let`, parameter,
    /// `for` variable) creates a unique slot. Reassignment reuses the existing
    /// slot. Shadowing (inner `let x`) creates a new slot; when the inner scope
    /// is popped, the outer slot is restored.
    pub scopes: Vec<HashMap<ast::Identifier, u32>>,

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

    /// Namespace aliases from `require` declarations.
    /// Maps call-site alias → extern namespace name.
    pub require_aliases: HashMap<ast::Identifier, ast::Identifier>,

    /// Extern functions merged into root scope via `require ns as _`.
    /// Maps function name → source namespace (for diagnostics).
    pub merged_externs: HashMap<ast::Identifier, ast::Identifier>,

    /// Expression-level guard fail block. When set, `emit_binary_intrinsic`
    /// and `emit_unary_intrinsic` guard their args against Undefined,
    /// jumping to this block if any arg is Undefined. All guards within
    /// an expression share the same fail block — no intermediate Phis.
    pub expr_guard_fail: Option<BlockId>,

    /// User function parameter by-ref modes, collected from AST before lowering.
    /// Used to emit Reload after calls for by-ref args from named variables.
    pub user_fn_params: HashMap<ast::Identifier, Vec<bool>>,
}

/// Context for a loop (for break/continue)
pub struct LoopContext {
    pub break_target: BlockId,
    pub continue_target: BlockId,
    pub break_values: Vec<(BlockId, VarId)>,
}

impl<'a> Lowerer<'a> {
    /// Create a new lowerer with the given extern registry and diagnostics
    pub fn new(externs: &'a externs::ExternRegistry, diagnostics: &'a mut Diagnostics) -> Self {
        Lowerer {
            externs,
            diagnostics,
            const_bindings: HashMap::new(),
            next_var_id: 0,
            next_block_id: 0,
            next_slot_id: 0,
            scopes: Vec::new(),
            ref_origins: Vec::new(),
            vars: Vec::new(),
            blocks: Vec::new(),
            current_block: BlockId(0),
            current_instructions: Vec::new(),
            current_span: ast::Span::default(),
            loop_stack: Vec::new(),
            require_aliases: HashMap::new(),
            merged_externs: HashMap::new(),
            expr_guard_fail: None,
            user_fn_params: HashMap::new(),
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
        self.emit_undefined()
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

    /// Get the declared TypeSet for a VarId.
    pub fn var_type(&self, var: VarId) -> TypeSet {
        self.vars
            .iter()
            .find(|v| v.id == var)
            .map(|v| v.type_set)
            .unwrap_or(TypeSet::any())
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

    // ========================================================================
    // Slot-based Variable Binding (pre-SSA)
    // ========================================================================

    /// Create a new variable slot and register it in the current scope.
    ///
    /// Each binding site (`let`, parameter, `for` variable, pattern binding)
    /// creates a unique slot. Returns the slot ID.
    pub fn new_slot(&mut self, name: &ast::Identifier) -> u32 {
        let slot = self.next_slot_id;
        self.next_slot_id += 1;
        // `_` is a discard binding — don't enter scope
        if name.0 != "_"
            && let Some(scope) = self.scopes.last_mut()
        {
            scope.insert(name.clone(), slot);
        }
        slot
    }

    /// Look up the slot ID for a variable name without emitting any instructions.
    pub fn lookup_slot(&self, name: &ast::Identifier) -> Option<u32> {
        for scope in self.scopes.iter().rev() {
            if let Some(&slot) = scope.get(name) {
                return Some(slot);
            }
        }
        None
    }

    /// Emit an `Assign` instruction: write `value` to `slot`.
    pub fn emit_assign(&mut self, slot: u32, value: VarId) {
        self.emit(Instruction::Assign { slot, value });
    }

    /// Emit a `Read` instruction: read the current value of `slot` into a
    /// fresh VarId. Returns the dest VarId.
    pub fn emit_read(&mut self, slot: u32) -> VarId {
        let dest = self.new_temp(TypeSet::any());
        self.emit(Instruction::Read { slot, dest });
        dest
    }

    /// Create a new binding and assign a value to it.
    ///
    /// Combines `new_slot` + `emit_assign`. Used for `let` bindings,
    /// parameters, and pattern bindings.
    pub fn bind(&mut self, name: &ast::Identifier, value: VarId) {
        let slot = self.new_slot(name);
        // Only emit Assign for non-discard bindings
        if name.0 != "_" {
            self.emit_assign(slot, value);
        }
    }

    /// Read a variable by name. Emits a `Read` instruction and returns
    /// the dest VarId. Returns `None` if the variable is not in scope.
    pub fn read_var(&mut self, name: &ast::Identifier) -> Option<VarId> {
        self.lookup_slot(name).map(|slot| self.emit_read(slot))
    }

    /// Reassign an existing variable by name. Emits an `Assign` instruction.
    /// Returns the slot ID, or `None` if the variable is not in scope.
    pub fn reassign(&mut self, name: &ast::Identifier, value: VarId) -> Option<u32> {
        if let Some(slot) = self.lookup_slot(name) {
            self.emit_assign(slot, value);
            Some(slot)
        } else {
            None
        }
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

    // ========================================================================
    // Typed Emission Helpers
    //
    // All computation flows through these. Each creates a temp VarId,
    // emits the instruction, and returns the VarId. The lowerer should
    // use these instead of constructing Instruction variants directly.
    // ========================================================================

    /// Emit a constant value, returning the temp that holds it.
    pub fn emit_const(&mut self, value: Literal) -> VarId {
        let type_set = match &value {
            Literal::Bool(_) => TypeSet::bool(),
            Literal::UInt(_) => TypeSet::uint(),
            Literal::Int(_) => TypeSet::int(),
            Literal::Float(_) => TypeSet::float(),
            Literal::Text(_) => TypeSet::text(),
            Literal::Bytes(_) => TypeSet::bytes(),
            Literal::Undefined => TypeSet::undefined(),
        };
        let dest = self.new_temp(type_set);
        self.emit(Instruction::Const { dest, value });
        dest
    }

    /// Emit a Copy, returning the new temp.
    pub fn emit_copy(&mut self, src: VarId, type_set: TypeSet) -> VarId {
        let dest = self.new_temp(type_set);
        self.emit(Instruction::Copy { dest, src });
        dest
    }

    /// Emit an Index operation: `dest = base[key]`.
    pub fn emit_index(&mut self, base: VarId, key: VarId) -> VarId {
        let dest = self.new_temp(TypeSet::any());
        self.emit(Instruction::Index { dest, base, key });
        dest
    }

    /// Emit a function call, returning the result temp.
    /// Uses the extern's declared return type if known, otherwise `any()`.
    pub fn emit_call(&mut self, function: FunctionRef, args: Vec<VarId>) -> VarId {
        let return_type = self
            .externs
            .lookup(&function)
            .map(|def| *def.meta.returns.type_sig())
            .unwrap_or(TypeSet::any());
        let dest = self.new_temp(return_type);
        self.emit(Instruction::Call {
            dest,
            function,
            args,
        });
        dest
    }

    /// Emit an Undefined constant.
    pub fn emit_undefined(&mut self) -> VarId {
        self.emit_const(Literal::Undefined)
    }

    /// Emit a Reload — SSA barrier after a mutation site.
    /// Creates a fresh VarId so subsequent reads see a new SSA definition.
    /// The source type is preserved (container type doesn't change, only contents).
    pub fn emit_reload(&mut self, src: VarId) -> VarId {
        let type_set = self.var_type(src);
        let dest = self.new_temp(type_set);
        self.emit(Instruction::Reload { dest, src });
        dest
    }

    /// Emit a Phi node, computing the type as the union of source types.
    pub fn emit_phi(&mut self, sources: Vec<(BlockId, VarId)>) -> VarId {
        let type_set = sources.iter().fold(TypeSet::none(), |acc, &(_, var)| {
            acc.union(&self.var_type(var))
        });
        let dest = self.new_temp(type_set);
        self.emit(Instruction::Phi { dest, sources });
        dest
    }

    /// Emit a binary intrinsic operation.
    /// If `expr_guard_fail` is set, type-guards both args against param_type.
    pub fn emit_binary_intrinsic(&mut self, op: IntrinsicOp, lhs: VarId, rhs: VarId) -> VarId {
        let (lhs, rhs) = if let Some(fail_bb) = self.expr_guard_fail {
            (
                self.emit_type_guard(lhs, op.param_type(0), fail_bb),
                self.emit_type_guard(rhs, op.param_type(1), fail_bb),
            )
        } else {
            (lhs, rhs)
        };
        let dest = self.new_temp(op.result_type());
        self.emit(Instruction::Intrinsic {
            dest,
            op,
            args: vec![lhs, rhs],
        });
        dest
    }

    /// Emit a unary intrinsic operation.
    /// If `expr_guard_fail` is set, type-guards the arg against param_type.
    pub fn emit_unary_intrinsic(&mut self, op: IntrinsicOp, arg: VarId) -> VarId {
        let arg = if let Some(fail_bb) = self.expr_guard_fail {
            self.emit_type_guard(arg, op.param_type(0), fail_bb)
        } else {
            arg
        };
        let dest = self.new_temp(op.result_type());
        self.emit(Instruction::Intrinsic {
            dest,
            op,
            args: vec![arg],
        });
        dest
    }

    /// Emit a type guard: Match on the value's type against the required TypeSet.
    /// If the value's type is in the required set, returns a narrowed VarId.
    /// If not, jumps to fail_bb.
    ///
    /// For single-type requirements (e.g. Bool), emits a single-arm Match.
    /// For multi-type requirements (e.g. numeric = UInt|Int|Float), emits
    /// a multi-arm Match with one arm per accepted type.
    pub fn emit_type_guard(&mut self, value: VarId, required: TypeSet, fail_bb: BlockId) -> VarId {
        // If the value's declared type is already within the required set,
        // no guard needed.
        let src_type = self.var_type(value);
        if !src_type.is_empty() && src_type.difference(&required).is_empty() {
            return value;
        }

        let ok_bb = self.fresh_block();

        // Build Match arms: one per accepted type
        let arms: Vec<(MatchPattern, BlockId)> = required
            .iter()
            .map(|ty| (MatchPattern::Type(ty), ok_bb))
            .collect();

        // Type guard is compiler-internal — use default span to suppress
        // spurious diagnostics about unreachable arms.
        self.finish_block(Terminator::Match {
            value,
            arms,
            default: fail_bb,
            span: ast::Span::default(),
        });

        self.current_block = ok_bb;
        self.current_instructions = Vec::new();

        // Narrowing: value is now known to be within the required type set
        self.emit_narrowing(value, src_type.intersection(&required))
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
// Public API
// ============================================================================

/// Lower an AST program to IR with the given extern registry
///
/// Errors are emitted to the diagnostics accumulator. Returns `Some(IrProgram)` if
/// lowering succeeded (possibly with warnings), `None` if there were errors.
pub fn lower(
    program: &ast::AstProgram,
    externs: &externs::ExternRegistry,
    diagnostics: &mut Diagnostics,
) -> Option<IrProgram> {
    let mut lowerer = Lowerer::new(externs, diagnostics);
    lowerer.lower_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Diagnostics;

    fn test_registry() -> externs::ExternRegistry {
        externs::standard_externs()
    }

    fn try_parse(source: &str) -> ast::AstProgram {
        let mut diags = Diagnostics::new();
        crate::ast::parser::parse(source, &mut diags).expect("parse failed")
    }

    fn try_lower(ast: &ast::AstProgram, registry: &externs::ExternRegistry) -> IrProgram {
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
