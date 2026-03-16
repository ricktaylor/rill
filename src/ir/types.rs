//! IR Type Definitions
//!
//! Single Static Assignment (SSA) form with type set tracking.
//!
//! Design Philosophy:
//! - All pattern matching (let, with, if let, if with, for, match) lowers to
//!   control flow primitives: Match (type dispatch), Guard (presence check),
//!   If (boolean branch), plus Index and Phi
//! - This enables standard optimizations: const-folding, dead code elimination,
//!   branch elimination, type narrowing
//! - Reference bindings (with) are tracked at compile time; at runtime all
//!   variables are stack slots, mutations go through captured base+key

use super::*;

// Re-export types from the shared types module
pub use crate::types::{BaseType, TypeSet};

// ============================================================================
// Builtin Operations
// ============================================================================

/// Intrinsic operations that require control flow (short-circuit evaluation)
///
/// These are the only operations that remain as intrinsics because they cannot
/// be implemented as simple function calls - they must control whether their
/// operands are evaluated.
///
/// All other operators (arithmetic, comparison, bitwise, etc.) are implemented
/// as `core.*` builtins with `Purity::Const(eval_fn)` for compile-time folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicOp {
    /// Logical AND with short-circuit evaluation
    And,

    /// Logical OR with short-circuit evaluation
    Or,
}

// ============================================================================
// SSA Variables
// ============================================================================

/// SSA variable identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VarId(pub u32);

/// SSA variable metadata
#[derive(Debug, Clone)]
pub struct Var {
    pub id: VarId,
    pub name: ast::Identifier,
    pub type_set: TypeSet,
}

impl Var {
    pub fn new(id: VarId, name: ast::Identifier, type_set: TypeSet) -> Self {
        Var { id, name, type_set }
    }
}

// ============================================================================
// IR Instructions (SSA form)
// ============================================================================

#[derive(Debug, Clone)]
pub enum Instruction {
    /// Phi node: merges values from different control flow paths
    Phi {
        dest: VarId,
        sources: Vec<(BlockId, VarId)>,
    },

    /// Copy a value (for let bindings, parameter passing)
    Copy { dest: VarId, src: VarId },

    /// Load a constant
    Const { dest: VarId, value: Literal },

    /// Load the "undefined" value
    Undefined { dest: VarId },

    /// Index into a collection: dest = base[key]
    Index {
        dest: VarId,
        base: VarId,
        key: VarId,
    },

    /// Set a value in a collection: base[key] = value
    SetIndex {
        base: VarId,
        key: VarId,
        value: VarId,
    },

    /// Intrinsic operation (pure, can be optimized)
    Intrinsic {
        dest: VarId,
        op: IntrinsicOp,
        args: Vec<VarId>,
    },

    /// User-defined function call (may have side effects)
    Call {
        dest: VarId,
        function: FunctionRef,
        args: Vec<CallArg>,
    },

    /// Create a reference binding for `with` statements
    MakeRef {
        dest: VarId,
        base: VarId,
        key: VarId,
    },

    /// Mark end of variable scope - slots can be reclaimed
    Drop { vars: Vec<VarId> },
}

/// Reference to a function (possibly namespaced)
#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub namespace: Option<ast::Identifier>,
    pub name: ast::Identifier,
}

impl FunctionRef {
    /// Create a FunctionRef for a core builtin (e.g., "add" -> core::add)
    pub fn core(name: &str) -> Self {
        FunctionRef {
            namespace: Some(ast::Identifier("core".to_string())),
            name: ast::Identifier(name.to_string()),
        }
    }

    /// Get the fully qualified name using `::` as separator
    ///
    /// This matches the naming convention used by the builtin registry.
    /// Examples: "core::add", "str::len", "my_function"
    pub fn qualified_name(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}::{}", ns, self.name),
            None => self.name.to_string(),
        }
    }
}

/// Argument to a function call with binding mode
#[derive(Debug, Clone)]
pub struct CallArg {
    pub value: VarId,
    pub by_ref: bool,
}

// ============================================================================
// Literals
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Bool(bool),
    UInt(u64),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

// ============================================================================
// Control Flow
// ============================================================================

/// Basic block identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// A spanned instruction for source location tracking
pub type SpannedInst = crate::ast::Spanned<Instruction>;

/// Basic block in SSA form
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: BlockId,
    pub instructions: Vec<SpannedInst>,
    pub terminator: Terminator,
}

/// Block terminator (control flow)
#[derive(Debug, Clone)]
pub enum Terminator {
    /// Unconditional jump
    Jump { target: BlockId },

    /// Branch on boolean condition
    If {
        condition: VarId,
        then_target: BlockId,
        else_target: BlockId,
        span: crate::ast::Span,
    },

    /// Dispatch on type (for type patterns)
    Match {
        value: VarId,
        arms: Vec<(MatchPattern, BlockId)>,
        default: BlockId,
        span: crate::ast::Span,
    },

    /// Branch on presence (for if let/if with, is_some checks)
    Guard {
        value: VarId,
        defined: BlockId,
        undefined: BlockId,
        span: crate::ast::Span,
    },

    /// Return from function
    Return { value: Option<VarId> },

    /// Hard exit to driver (from diverging builtins like drop())
    Exit { value: VarId },

    /// Unreachable code (placeholder after merging)
    Unreachable,
}

impl Terminator {
    /// Returns all successor block IDs for this terminator
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Jump { target } => vec![*target],
            Terminator::If {
                then_target,
                else_target,
                ..
            } => vec![*then_target, *else_target],
            Terminator::Match { arms, default, .. } => {
                let mut succs: Vec<BlockId> = arms.iter().map(|(_, b)| *b).collect();
                succs.push(*default);
                succs
            }
            Terminator::Guard {
                defined, undefined, ..
            } => vec![*defined, *undefined],
            Terminator::Return { .. } | Terminator::Exit { .. } | Terminator::Unreachable => {
                vec![]
            }
        }
    }
}

/// Pattern for Match terminator arms
#[derive(Debug, Clone)]
pub enum MatchPattern {
    /// Match a specific literal value
    Literal(Literal),

    /// Match a simple type
    Type(BaseType),

    /// Match array with exact length
    Array(usize),

    /// Match array with minimum length (for rest patterns)
    ArrayMin(usize),
}

// ============================================================================
// Functions and Programs
// ============================================================================

/// IR function
#[derive(Debug, Clone)]
pub struct Function {
    pub name: ast::Identifier,
    pub attributes: Vec<ast::Attribute>,
    pub params: Vec<Param>,
    pub rest_param: Option<Param>,
    pub locals: Vec<Var>,
    pub blocks: Vec<BasicBlock>,
    pub entry_block: BlockId,
}

/// Function parameter with binding mode
#[derive(Debug, Clone)]
pub struct Param {
    pub var: VarId,
    pub by_ref: bool,
}

/// Complete IR program
#[derive(Debug, Clone)]
pub struct IrProgram {
    pub functions: Vec<Function>,
    pub constants: Vec<ConstBinding>,
    pub imports: Vec<Import>,
}

/// A constant binding (result of const pattern matching)
#[derive(Debug, Clone)]
pub struct ConstBinding {
    pub name: ast::Identifier,
    pub value: ConstValue,
}

/// Import declaration for module resolution
#[derive(Debug, Clone)]
pub struct Import {
    pub namespace: ast::Identifier,
    pub path: ast::ImportPath,
}

/// Compile-time evaluated constant value
#[derive(Debug, Clone, PartialEq)]
pub enum ConstValue {
    Bool(bool),
    UInt(u64),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<ConstValue>),
    Map(Vec<(ConstValue, ConstValue)>),
}
