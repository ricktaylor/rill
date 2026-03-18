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
    // Note: && and || lower to control flow (If + Phi), not Intrinsic instructions.
    Not,

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
    SeqNext,

    // -- Collection/Sequence --
    /// Materialize a Sequence into an Array by draining all remaining elements.
    Collect,

    // -- Coercion --
    /// Explicit numeric type widening along the promotion lattice.
    ///
    /// `Widen(value, target)` where target is a UInt constant encoding a BaseType:
    /// - `1` (UInt) — no-op (identity)
    /// - `2` (Int) — UInt→Int
    /// - `3` (Float) — UInt→Float or Int→Float
    ///
    /// Making coercion explicit enables the optimizer to fold, hoist, and
    /// eliminate widening operations. Currently implicit inside each
    /// arithmetic op's type dispatch.
    Widen,

    /// Infallible numeric cast (`value as Type`).
    ///
    /// `Cast(value, target)` where target is a UInt constant encoding a BaseType:
    /// - `1` (UInt) — Int→UInt bit reinterpret, UInt identity
    /// - `2` (Int) — UInt→Int bit reinterpret, Int identity
    /// - `3` (Float) — UInt→Float or Int→Float widen, Float identity
    ///
    /// Unlike Widen (which is compiler-inserted and overflow-checked), Cast is
    /// user-requested and always succeeds for valid numeric pairs.
    Cast,
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
            Self::Not => false,
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
            Self::SeqNext => true,  // exhausted → undefined
            Self::Collect => false, // always succeeds (empty seq → empty array)
            // Coercion: UInt→Int can overflow (u64::MAX > i64::MAX)
            Self::Widen => true,
            // Cast: infallible for valid numeric pairs
            Self::Cast => false,
        }
    }

    /// Required type for each argument position.
    ///
    /// Returns the TypeSet of types that are valid for each argument. If the
    /// actual operand type has no intersection with the required type, the
    /// operation will always produce undefined — which is almost certainly a bug.
    pub fn param_type(self, index: usize) -> TypeSet {
        match self {
            // Arithmetic: both args must be numeric
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod => TypeSet::numeric(),
            Self::Neg => TypeSet::numeric(),

            // Comparison
            Self::Eq => TypeSet::all(), // any two values can be compared
            Self::Lt => TypeSet::numeric(),

            // Logical: Bool only
            Self::Not => TypeSet::bool(),

            // Bitwise: UInt only
            Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::BitNot
            | Self::Shl
            | Self::Shr
            | Self::BitTest => TypeSet::uint(),
            Self::BitSet => match index {
                0 | 1 => TypeSet::uint(), // x and bit position
                _ => TypeSet::bool(),     // value to set
            },

            // Collection
            Self::Len => TypeSet::collection(),
            Self::MakeArray | Self::MakeMap => TypeSet::all(),

            // Sequence
            Self::MakeSeq => TypeSet::numeric(), // start/end are numeric
            Self::ArraySeq => TypeSet::all(),
            Self::SeqNext => TypeSet::single(BaseType::Sequence), // arg must be Sequence
            Self::Collect => TypeSet::single(BaseType::Sequence),
            // Coercion
            Self::Widen => match index {
                0 => TypeSet::numeric(), // value to widen
                _ => TypeSet::uint(),    // target type code
            },
            Self::Cast => match index {
                0 => TypeSet::numeric(), // value to cast
                _ => TypeSet::uint(),    // target type code
            },
        }
    }

    /// Static result type (worst case, ignoring operand types).
    pub fn result_type(self) -> TypeSet {
        match self {
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod | Self::Neg => {
                TypeSet::numeric()
            }
            Self::Eq | Self::Lt | Self::Not | Self::BitTest => TypeSet::bool(),
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
            Self::SeqNext => TypeSet::all(), // element could be any type
            Self::Collect => TypeSet::single(BaseType::Array),
            Self::Widen => TypeSet::numeric(), // result is Int or Float
            Self::Cast => TypeSet::numeric(),  // result is UInt, Int, or Float
        }
    }

    /// Refined result type given known operand types.
    ///
    /// For arithmetic ops, the result type follows the numeric promotion
    /// lattice: UInt + UInt → UInt, UInt + Int → Int, anything + Float → Float.
    /// If operand types are unknown or mixed, falls back to `result_type()`.
    pub fn result_type_refined(self, arg_types: &[TypeSet]) -> TypeSet {
        match self {
            // Arithmetic: result type follows promotion rules
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod => {
                if let (Some(a), Some(b)) = (arg_types.first(), arg_types.get(1)) {
                    numeric_result_type(*a, *b)
                } else {
                    self.result_type()
                }
            }
            Self::Neg => {
                if let Some(a) = arg_types.first()
                    && a.is_single()
                {
                    if a.contains(BaseType::UInt) || a.contains(BaseType::Int) {
                        // neg(UInt) → Int, neg(Int) → Int
                        return TypeSet::single(BaseType::Int);
                    }
                    if a.contains(BaseType::Float) {
                        return TypeSet::single(BaseType::Float);
                    }
                }
                self.result_type()
            }
            // Comparison: result type follows promotion for the comparison,
            // but the output is always Bool
            Self::Eq | Self::Lt | Self::Not | Self::BitTest => TypeSet::bool(),
            // Everything else has a fixed result type regardless of operands
            _ => self.result_type(),
        }
    }
}

/// Compute the numeric result type given two operand TypeSets.
///
/// Follows the promotion lattice: UInt ⊂ Int ⊂ Float.
/// - Same type → same type (UInt+UInt → UInt)
/// - Mixed integers → Int (UInt+Int → Int)
/// - Anything + Float → Float
/// - Non-numeric or ambiguous → numeric() (all three)
fn numeric_result_type(a: TypeSet, b: TypeSet) -> TypeSet {
    // Both must be single numeric types for precise refinement
    if !a.is_single() || !b.is_single() {
        // If both are subsets of numeric, the result is at most numeric
        let numeric = TypeSet::numeric();
        if a.intersection(&numeric) == a && b.intersection(&numeric) == b {
            // Compute the union of possible result types from the promotion lattice
            return promote_union(a, b);
        }
        return TypeSet::numeric();
    }

    let a_has = |t| a.contains(t);
    let b_has = |t| b.contains(t);

    // Float + anything → Float
    if a_has(BaseType::Float) || b_has(BaseType::Float) {
        return TypeSet::single(BaseType::Float);
    }
    // Int + UInt → Int, Int + Int → Int
    if a_has(BaseType::Int) || b_has(BaseType::Int) {
        return TypeSet::single(BaseType::Int);
    }
    // UInt + UInt → UInt
    if a_has(BaseType::UInt) && b_has(BaseType::UInt) {
        return TypeSet::single(BaseType::UInt);
    }
    TypeSet::numeric()
}

/// Compute the union of possible promoted types when operands have multi-type sets.
fn promote_union(a: TypeSet, b: TypeSet) -> TypeSet {
    let mut result = TypeSet::empty();

    let a_u = a.contains(BaseType::UInt);
    let a_i = a.contains(BaseType::Int);
    let a_f = a.contains(BaseType::Float);
    let b_u = b.contains(BaseType::UInt);
    let b_i = b.contains(BaseType::Int);
    let b_f = b.contains(BaseType::Float);

    // UInt + UInt → UInt
    if a_u && b_u {
        result = result.union(&TypeSet::single(BaseType::UInt));
    }
    // Int + Int, UInt + Int, Int + UInt → Int
    if (a_u || a_i) && b_i || (a_i && b_u) {
        result = result.union(&TypeSet::single(BaseType::Int));
    }
    // Float + anything numeric → Float
    if a_f || b_f {
        result = result.union(&TypeSet::single(BaseType::Float));
    }

    if result.is_empty() {
        TypeSet::numeric()
    } else {
        result
    }
}

/// Map a user-callable function name to its IntrinsicOp, if it's a
/// language-defined intrinsic rather than a host-provided extern.
pub fn intrinsic_by_name(name: &str) -> Option<IntrinsicOp> {
    match name {
        "len" => Some(IntrinsicOp::Len),
        "collect" => Some(IntrinsicOp::Collect),
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
#[allow(dead_code)]
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

    /// Create a reference binding for `with` statements.
    ///
    /// Reads the value at `base[key]` (element ref) or `base` (whole-value ref)
    /// into `dest`, and records that `dest` is a reference to that location.
    /// The optimizer uses this provenance to reason about write-back semantics.
    ///
    /// - `key: Some(k)` — element reference: `with x = arr[i]`
    /// - `key: None` — whole-value reference: `with x = y`
    MakeRef {
        dest: VarId,
        base: VarId,
        key: Option<VarId>,
    },

    /// Write through a reference created by MakeRef.
    ///
    /// Semantically: writes `value` back to the location that `ref_var` references.
    /// The compiler resolves `ref_var` to its MakeRef to find (base, key) and
    /// emits the appropriate SetIndex or slot write.
    ///
    /// This instruction has no `dest` — it is a side effect (mutating a collection
    /// or variable through a reference). The optimizer can see these explicitly
    /// and reason about dead write-backs, forwarding, etc.
    WriteRef { ref_var: VarId, value: VarId },

    /// Mark end of variable scope - slots can be reclaimed (planned)
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
#[allow(dead_code)]
pub struct Param {
    pub var: VarId,
    pub by_ref: bool,
}

/// Complete IR program
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IrProgram {
    pub functions: Vec<Function>,
    pub constants: Vec<ConstBinding>,
    pub imports: Vec<Import>,
}

/// A constant binding (result of const pattern matching)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConstBinding {
    pub name: ast::Identifier,
    pub value: ConstValue,
}

/// Import declaration for module resolution
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
