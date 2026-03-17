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
// Intrinsic Operations
// ============================================================================

/// Language-defined operations with fixed semantics.
///
/// These are "processor instructions" — the compiler knows their exact
/// semantics, arity, types, and const-eval behavior. They are never
/// user-callable by name; they exist only as lowering targets for syntax.
///
/// Separating intrinsics from the `BuiltinRegistry` enables:
/// - Type-specialized code generation (e.g., `Add` on two `UInt` values
///   compiles to a single `u64::checked_add`, not a 10-way type dispatch)
/// - Peephole optimization via a `StepKind` intermediate
/// - A clean `BuiltinRegistry` containing only host-provided extern functions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicOp {
    // -- Arithmetic --
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,

    // -- Comparison --
    Eq,
    Lt,

    // -- Logical --
    Not,
    And,
    Or,

    // -- Bitwise --
    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    Shl,
    Shr,
    BitTest,
    BitSet,

    // -- Collection --
    Len,
    MakeArray,
    MakeMap,

    // -- Sequence --
    MakeSeq,
    ArraySeq,
}

impl IntrinsicOp {
    /// Whether this operation can fail (return undefined) for domain reasons.
    /// Impure operations are always fallible; this covers the pure/const case.
    pub fn is_fallible(self) -> bool {
        match self {
            // Arithmetic can overflow / divide-by-zero
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod | Self::Neg => true,
            // Comparison: type mismatch → undefined
            Self::Eq => false,
            Self::Lt => true,
            // Logical: always succeed on correct types
            Self::Not | Self::And | Self::Or => false,
            // Bitwise: bit_test/bit_set can go out of bounds
            Self::BitAnd | Self::BitOr | Self::BitXor | Self::BitNot | Self::Shl | Self::Shr => {
                false
            }
            Self::BitTest | Self::BitSet => true,
            // Collection
            Self::Len => true, // wrong type → undefined
            Self::MakeArray => false,
            Self::MakeMap => true, // odd arg count
            // Sequence
            Self::MakeSeq | Self::ArraySeq => false,
        }
    }

    /// Result type hint (before type refinement narrows further).
    pub fn result_type(self) -> TypeSet {
        match self {
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod | Self::Neg => {
                TypeSet::numeric()
            }
            Self::Eq | Self::Lt | Self::Not | Self::And | Self::Or | Self::BitTest => {
                TypeSet::bool()
            }
            Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::BitNot
            | Self::Shl
            | Self::Shr
            | Self::BitSet
            | Self::Len => TypeSet::uint(),
            Self::MakeArray => TypeSet::single(BaseType::Array),
            Self::MakeMap => TypeSet::single(BaseType::Map),
            Self::MakeSeq | Self::ArraySeq => TypeSet::single(BaseType::Sequence),
        }
    }
}

/// Map a user-callable function name to its IntrinsicOp, if it's a
/// language-defined intrinsic rather than a host-provided extern.
pub fn intrinsic_by_name(name: &str) -> Option<IntrinsicOp> {
    match name {
        "len" => Some(IntrinsicOp::Len),
        _ => None,
    }
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

impl BasicBlock {
    /// Create a block with the given id, empty instructions, and Unreachable terminator.
    pub fn new(id: BlockId) -> Self {
        BasicBlock {
            id,
            instructions: Vec::new(),
            terminator: Terminator::Unreachable,
        }
    }
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

impl Default for Function {
    fn default() -> Self {
        Function {
            name: ast::Identifier("_".to_string()),
            attributes: Vec::new(),
            params: Vec::new(),
            rest_param: None,
            locals: Vec::new(),
            blocks: Vec::new(),
            entry_block: BlockId(0),
        }
    }
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
