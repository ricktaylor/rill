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
use std::collections::BTreeSet;

// Re-export BaseType so submodules can access it via types::BaseType
pub use crate::types::BaseType;

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

/// Set of possible types for an SSA value
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeSet {
    pub types: BTreeSet<BaseType>,
    pub maybe_undefined: bool,
}

impl TypeSet {
    /// Create a type set containing only one type (not undefined)
    pub fn single(ty: BaseType) -> Self {
        let mut types = BTreeSet::new();
        types.insert(ty);
        TypeSet {
            types,
            maybe_undefined: false,
        }
    }

    /// Create a type set that is just "undefined" (no concrete types)
    pub fn undefined() -> Self {
        TypeSet {
            types: BTreeSet::new(),
            maybe_undefined: true,
        }
    }

    /// Create a type set from multiple types (not undefined)
    pub fn from_types(types: impl IntoIterator<Item = BaseType>) -> Self {
        TypeSet {
            types: types.into_iter().collect(),
            maybe_undefined: false,
        }
    }

    /// Union of two type sets (for phi nodes)
    pub fn union(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            types: self.types.union(&other.types).copied().collect(),
            maybe_undefined: self.maybe_undefined || other.maybe_undefined,
        }
    }

    /// Intersection of two type sets
    pub fn intersection(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            types: self.types.intersection(&other.types).copied().collect(),
            maybe_undefined: self.maybe_undefined && other.maybe_undefined,
        }
    }

    /// Type after successful presence check - guarantees value is defined
    pub fn unwrapped(&self) -> TypeSet {
        TypeSet {
            types: self.types.clone(),
            maybe_undefined: false,
        }
    }

    /// Make this type optional (might be undefined)
    pub fn as_optional(&self) -> TypeSet {
        TypeSet {
            types: self.types.clone(),
            maybe_undefined: true,
        }
    }

    /// Check if type set includes a specific type
    pub fn contains(&self, ty: BaseType) -> bool {
        self.types.contains(&ty)
    }

    /// Check if type set is empty (unreachable code)
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && !self.maybe_undefined
    }

    /// Check if this type set can be used in a boolean context
    pub fn is_boolean(&self) -> bool {
        self.types.len() == 1 && self.types.contains(&BaseType::Bool) && !self.maybe_undefined
    }

    /// Check if this type set contains only numeric types
    pub fn is_numeric(&self) -> bool {
        !self.types.is_empty()
            && self
                .types
                .iter()
                .all(|t| matches!(t, BaseType::UInt | BaseType::Int | BaseType::Float))
    }

    /// Check if this value might be undefined
    pub fn is_optional(&self) -> bool {
        self.maybe_undefined
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

    /// Conditional select: dest = cond ? then_val : else_val
    Select {
        dest: VarId,
        cond: VarId,
        then_val: VarId,
        else_val: VarId,
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

/// Argument to a function call with binding mode
#[derive(Debug, Clone)]
pub struct CallArg {
    pub value: VarId,
    pub by_ref: bool,
}

// ============================================================================
// Literals
// ============================================================================

#[derive(Debug, Clone)]
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

/// Basic block in SSA form
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: BlockId,
    pub instructions: Vec<Instruction>,
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
    },

    /// Dispatch on type (for type patterns)
    Match {
        value: VarId,
        arms: Vec<(MatchPattern, BlockId)>,
        default: BlockId,
    },

    /// Branch on presence (for if let/if with, is_some checks)
    Guard {
        value: VarId,
        defined: BlockId,
        undefined: BlockId,
    },

    /// Return from function
    Return { value: Option<VarId> },

    /// Hard exit to driver (from diverging builtins like drop())
    Exit { value: VarId },
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
pub struct Program {
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
