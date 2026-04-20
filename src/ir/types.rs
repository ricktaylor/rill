//! IR Type Definitions
//!
//! Single Static Assignment (SSA) form with type set tracking.
//!
//! Design Philosophy:
//! - All pattern matching (let, with, if let, if with, for, match) lowers to
//!   control flow primitives: Match (type dispatch, including Undefined checks),
//!   If (boolean branch), plus Index and Phi
//! - This enables standard optimizations: const-folding, dead code elimination,
//!   branch elimination, type narrowing
//! - Reference bindings (with) are tracked at compile time; at runtime all
//!   variables are stack slots, mutations go through captured base+key

use super::*;

// Re-export types from the shared types module
pub use crate::types::{BaseType, ConvertMode, NumericType, SliceMode, TypeSet};

// ============================================================================
// Intrinsic Operations
// ============================================================================

/// Language-defined operations with fixed semantics.
///
/// These are "processor instructions" — the compiler knows their exact
/// semantics, arity, types, and const-eval behavior. They are never
/// user-callable by name; they exist only as lowering targets for syntax.
///
/// Separating intrinsics from the `ExternRegistry` enables:
/// - Type-specialized code generation (e.g., `Add` on two `UInt` values
///   compiles to a single `u64::checked_add`, not a 10-way type dispatch)
/// - Peephole optimization via a `StepKind` intermediate
/// - A clean `ExternRegistry` containing only host-provided extern functions
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
    /// Create a lazy numeric sequence with exclusive end.
    ///
    /// Inclusive ranges (`..=`) are normalized by the lowerer: it emits
    /// `end + 1` (checked Add) before `MakeSeq`, so overflow on
    /// `0..=u64::MAX` produces undefined naturally.
    MakeSeq,
    /// Create a zero-copy array slice sequence. The `SliceMode` determines
    /// mutability — known at compile time from `let` vs `with` binding.
    ArraySeq(SliceMode),
    SeqNext,

    // -- Collection/Sequence --
    /// Materialize a Sequence into an Array by draining all remaining elements.
    Collect,

    // -- Coercion --
    /// Numeric type conversion — single-arg intrinsic.
    ///
    /// `Convert(target, mode, [value])` — target type and mode are compile-time
    /// properties of the instruction, not runtime operands.
    ///
    /// - `Checked`: compiler-inserted promotion (coercion pass). UInt→Int is
    ///   overflow-checked. Only goes "up" the widening lattice.
    /// - `Unchecked`: user `as Type` syntax. Bit-reinterprets Int↔UInt,
    ///   always succeeds for valid numeric pairs.
    Convert(NumericType, ConvertMode),
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
            Self::MakeSeq | Self::ArraySeq(_) => false,
            Self::SeqNext => true,  // exhausted → undefined
            Self::Collect => false, // always succeeds (empty seq → empty array)
            // Checked UInt→Int can overflow (u64::MAX > i64::MAX), all others are infallible
            Self::Convert(NumericType::Int, ConvertMode::Checked) => true,
            Self::Convert(..) => false,
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
            Self::Eq => TypeSet::any(), // any two values can be compared
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
            Self::MakeArray | Self::MakeMap => TypeSet::any(),

            // Sequence
            Self::MakeSeq => TypeSet::uint(), // start, end
            Self::ArraySeq(_) => TypeSet::any(),
            Self::SeqNext => TypeSet::single(BaseType::Sequence), // arg must be Sequence
            Self::Collect => TypeSet::single(BaseType::Sequence),
            // Conversion: single arg (the value to convert)
            Self::Convert(..) => TypeSet::numeric(),
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
            Self::MakeSeq | Self::ArraySeq(_) => TypeSet::single(BaseType::Sequence),
            Self::SeqNext => TypeSet::any(), // element could be any type
            Self::Collect => TypeSet::single(BaseType::Array),
            Self::Convert(t, _) => TypeSet::single(BaseType::from(t)),
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
    let mut result = TypeSet::none();

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

    if result.is_dead() {
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

/// Names reserved by the compiler — intrinsics and special instructions.
/// Used for name clash detection during lowering.
pub fn is_reserved_name(name: &str) -> bool {
    matches!(name, "len" | "collect" | "append")
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

    /// Load a constant (including `Literal::Undefined` for absent values)
    Const { dest: VarId, value: Literal },

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

    /// Append a value to an array in place: `append(arr, val)`.
    ///
    /// Mutates `arr` via CoW. Like `SetIndex`, this is a side-effecting
    /// instruction — not a pure intrinsic. `dest` receives the array
    /// after mutation (for result capture), or undefined if `arr` is not
    /// an Array.
    Append {
        dest: VarId,
        arr: VarId,
        value: VarId,
    },

    /// Write to a variable slot (pre-SSA form).
    ///
    /// Records that `slot` holds `value` at this point. Each binding site
    /// (`let`, parameter, `for` variable) creates a unique slot ID. Reassignment
    /// reuses the existing slot. Consumed by mem2reg, which converts these
    /// to SSA VarIds with Phi nodes.
    Assign { slot: u32, value: VarId },

    /// Read from a variable slot (pre-SSA form).
    ///
    /// Produces the current value of `slot` into `dest`. Consumed by mem2reg,
    /// which resolves each Read to the reaching definition (possibly through
    /// Phi nodes at merge points).
    Read { slot: u32, dest: VarId },

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
    /// Create a FunctionRef for an extern (e.g., "math::sqrt")
    pub fn core(name: &str) -> Self {
        FunctionRef {
            namespace: Some(ast::Identifier("core".to_string())),
            name: ast::Identifier(name.to_string()),
        }
    }

    /// Get the fully qualified name using `::` as separator
    ///
    /// This matches the naming convention used by the extern registry.
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
    Undefined,
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

    /// Return from function
    Return { value: Option<VarId> },

    /// Hard exit to driver (from diverging externs like drop())
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
pub struct IrProgram {
    pub functions: Vec<Function>,
    pub constants: Vec<ConstBinding>,
}

/// A constant binding (result of const pattern matching)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConstBinding {
    pub name: ast::Identifier,
    pub value: ConstValue,
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
