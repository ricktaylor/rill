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
    /// `MapKeyAt(map, i)` → the i-th key of a Map in insertion order.
    /// Used by `for k, v in map` lowering for positional key access.
    MapKeyAt,

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
    /// Required type for each argument position.
    ///
    /// All intrinsics require defined inputs — Undefined poisons everything.
    /// No param_type includes Undefined.
    pub fn param_type(self, index: usize) -> TypeSet {
        match self {
            // Arithmetic: both args must be numeric
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod => TypeSet::numeric(),
            Self::Neg => TypeSet::numeric(),

            // Comparison: any defined value
            Self::Eq => TypeSet::defined(),
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
            Self::MakeArray | Self::MakeMap => TypeSet::defined(),
            Self::MapKeyAt => match index {
                0 => TypeSet::map(), // the map
                _ => TypeSet::uint(), // positional index
            },

            // Sequence
            Self::MakeSeq => TypeSet::uint(), // start, end
            Self::ArraySeq(_) => TypeSet::defined(),
            Self::SeqNext => TypeSet::single(BaseType::Sequence),
            Self::Collect => TypeSet::single(BaseType::Sequence),
            // Conversion: single arg (the value to convert)
            Self::Convert(..) => TypeSet::numeric(),
        }
    }

    /// Static result type — the value types this operation can produce.
    ///
    /// Includes Undefined where the operation can fail (overflow, div-by-zero,
    /// type mismatch, out-of-bounds, exhaustion). Expression-level guards
    /// ensure inputs are defined, but the operation itself may still produce
    /// Undefined from domain errors.
    pub fn result_type(self) -> TypeSet {
        match self {
            // Arithmetic: can overflow / div-by-zero
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod | Self::Neg => {
                TypeSet::numeric().union(&TypeSet::undefined())
            }
            // Eq: infallible (with defined inputs, always produces Bool)
            Self::Eq | Self::Not => TypeSet::bool(),
            // Lt: type mismatch → undefined
            Self::Lt => TypeSet::bool().union(&TypeSet::undefined()),
            // Bitwise and/or/xor/not: infallible on UInt inputs
            Self::BitAnd | Self::BitOr | Self::BitXor | Self::BitNot => TypeSet::uint(),
            // Shifts: amount >= 64 → undefined (checked, like arithmetic)
            Self::Shl | Self::Shr => TypeSet::uint().union(&TypeSet::undefined()),
            // BitTest/BitSet: out-of-bounds bit position → undefined
            Self::BitTest => TypeSet::bool().union(&TypeSet::undefined()),
            Self::BitSet => TypeSet::uint().union(&TypeSet::undefined()),
            // Len: wrong type → undefined
            Self::Len => TypeSet::uint().union(&TypeSet::undefined()),
            // MapKeyAt: a key (any defined value), or Undefined if out of bounds
            Self::MapKeyAt => TypeSet::defined().union(&TypeSet::undefined()),
            // Collection construction: infallible (lowerer guarantees valid args)
            Self::MakeArray => TypeSet::array(),
            Self::MakeMap => TypeSet::map(),
            // Sequence: MakeSeq/ArraySeq infallible, SeqNext exhaustion → undefined
            Self::MakeSeq | Self::ArraySeq(_) => TypeSet::sequence(),
            Self::SeqNext => TypeSet::any(),
            // Collect: always succeeds (empty seq → empty array)
            Self::Collect => TypeSet::array(),
            // Convert: checked UInt→Int can overflow
            Self::Convert(NumericType::Int, ConvertMode::Checked) => {
                TypeSet::int().union(&TypeSet::undefined())
            }
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

    if result.is_empty() {
        TypeSet::numeric()
    } else {
        result
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// Whether this is a user-defined variable (not a compiler temp).
    pub fn is_user_var(&self) -> bool {
        let n = self.name.as_ref();
        !n.starts_with('$') && n != "_phi"
    }

    /// Human-readable name for diagnostics.
    /// Returns the original variable name for user vars, or `_N` for temps.
    pub fn display_name(&self) -> String {
        if self.is_user_var() {
            format!("`{}`", self.name)
        } else {
            format!("_{}", self.id.0)
        }
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
        args: Vec<VarId>,
    },

    /// Create an Accessor to a collection element (`with x = arr[i]`).
    ///
    /// At runtime, creates `Slot::Accessor { base, key }` in the dest slot.
    /// Reading through the Accessor indexes into the collection.
    /// Writing through the Accessor mutates the collection element.
    MakeAccessor {
        dest: VarId,
        base: VarId,
        key: VarId,
    },

    /// Create a Ref to another slot (`with x = y`, or by-ref function params).
    ///
    /// At runtime, creates `Slot::Ref(target)` with path compression.
    /// Reading follows the Ref. Writing follows the Ref (and through any
    /// Accessor at the target, enabling Ref→Accessor chaining).
    MakeRef { dest: VarId, base: VarId },

    /// Write a value to a collection element: base[key] = value.
    ///
    /// Direct collection mutation — compiles to a type-specialized
    /// write closure (Array or Map). No Slot::Accessor dispatch.
    /// Used for direct indexed assignment (`arr[i] = val`).
    ///
    /// This instruction has no `dest` — it is a side effect.
    WriteAccessor {
        base: VarId,
        key: VarId,
        value: VarId,
    },

    /// Write through a Ref or Accessor binding.
    ///
    /// At runtime: `vm.set_local(slot(ref_var), value)` which resolves
    /// through Ref chains and Accessors automatically.
    /// Used for writes to `with`-bound variables and by-ref params.
    ///
    /// This instruction has no `dest` — it is a side effect.
    WriteRef { ref_var: VarId, value: VarId },

    /// Append a value to an array in place: `append(arr, val)`.
    ///
    /// Mutates `arr` via CoW. This is a side-effecting
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

    /// Reload a potentially-mutated value into a fresh VarId.
    ///
    /// Opaque to mem2reg — creates an SSA barrier so that subsequent reads
    /// of the variable get a new VarId after a mutation site (WriteRef,
    /// Append, or by-ref function call). At runtime, this is
    /// a slot copy (reads the current value of src's slot).
    Reload { dest: VarId, src: VarId },

    /// Read a file-scope global into a fresh SSA VarId: `dest = global[slot]`.
    ///
    /// `slot` is an absolute VM stack index (globals occupy slots 0..N).
    /// Each load produces a fresh VarId — the global may change between loads
    /// due to intervening calls — so it is never CSE'd. Pure (no side effect):
    /// removable if `dest` is unused.
    LoadGlobal { dest: VarId, slot: u32 },

    /// Write a value to a file-scope global: `global[slot] = value`.
    ///
    /// `slot` is an absolute VM stack index. This is a side effect (no `dest`)
    /// and is never eliminated.
    StoreGlobal { slot: u32, value: VarId },
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

impl Literal {
    /// The BaseType this literal belongs to.
    pub fn base_type(&self) -> BaseType {
        match self {
            Literal::Bool(_) => BaseType::Bool,
            Literal::UInt(_) => BaseType::UInt,
            Literal::Int(_) => BaseType::Int,
            Literal::Float(_) => BaseType::Float,
            Literal::Text(_) => BaseType::Text,
            Literal::Bytes(_) => BaseType::Bytes,
            Literal::Undefined => BaseType::Undefined,
        }
    }
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

    /// Unreachable code (placeholder after merging)
    Unreachable,

    /// Self-recursive tail call: overwrite params and jump to entry.
    /// Introduced by the TCO pass; has no successors (replaces Call + Return chain).
    /// Currently self-recursive only — the target is always the enclosing function.
    TailCall { args: Vec<VarId> },
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
            Terminator::Return { .. } | Terminator::Unreachable | Terminator::TailCall { .. } => {
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
    pub params: Vec<VarId>,
    pub rest_param: Option<VarId>,
    pub locals: Vec<Var>,
    pub blocks: Vec<BasicBlock>,
    pub entry_block: BlockId,
}

impl Function {
    /// Get the TypeSet of a variable by VarId. O(1) direct indexing.
    pub fn var_type(&self, var: VarId) -> TypeSet {
        self.locals
            .get(var.0 as usize)
            .map(|v| v.type_set)
            .unwrap_or(TypeSet::any())
    }

    /// Human-readable variable name for diagnostics.
    /// For user variables, returns the name. For temps, traces back through
    /// SSA to describe the expression that produced the value.
    pub fn var_display_name(&self, var: VarId) -> String {
        if let Some(v) = self.locals.get(var.0 as usize)
            && v.is_user_var()
        {
            return v.display_name();
        }
        // Temp — find the defining instruction and describe it
        self.describe_var_origin(var, 0)
    }

    /// Trace a VarId back through SSA to describe its origin. `depth` guards
    /// against Copy chains that cycle (the diagnostic must always terminate).
    fn describe_var_origin(&self, var: VarId, depth: usize) -> String {
        if depth > 32 {
            return format!("_{}", var.0);
        }
        for block in &self.blocks {
            for inst in &block.instructions {
                let (dest, desc) = match &inst.node {
                    Instruction::Copy { dest, src } => {
                        // Follow copies to find the real origin
                        let src = *src;
                        let src_desc = if self
                            .locals
                            .get(src.0 as usize)
                            .is_some_and(|v| v.is_user_var())
                        {
                            self.locals[src.0 as usize].display_name()
                        } else {
                            self.describe_var_origin(src, depth + 1)
                        };
                        (*dest, src_desc)
                    }
                    Instruction::Index { dest, .. } => (*dest, "index result".to_string()),
                    Instruction::Intrinsic { dest, op, .. } => {
                        (*dest, format!("result of `{:?}`", op))
                    }
                    Instruction::Call { dest, function, .. } => (
                        *dest,
                        format!("result of call to `{}`", function.qualified_name()),
                    ),
                    Instruction::Phi { dest, .. } => (*dest, "merged value".to_string()),
                    Instruction::MakeRef { dest, .. } => (*dest, "reference".to_string()),
                    Instruction::Append { dest, .. } => (*dest, "append result".to_string()),
                    Instruction::Const { dest, .. } => (*dest, "constant".to_string()),
                    _ => continue,
                };
                if dest == var {
                    return desc;
                }
            }
        }
        format!("_{}", var.0)
    }
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

// ============================================================================
// IR Dump (Debug)
// ============================================================================

#[cfg(test)]
impl Function {
    /// Dump the function's IR to a string for debugging.
    pub fn dump(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        let params: Vec<String> = self.params.iter().map(|v| format!("v{}", v.0)).collect();
        let _ = writeln!(out, "fn {}({}):", self.name, params.join(", "));

        for block in &self.blocks {
            let _ = writeln!(out, "  B{}:", block.id.0);
            for inst in &block.instructions {
                let _ = writeln!(out, "    {}", fmt_instruction(&inst.node));
            }
            let _ = writeln!(out, "    → {}", fmt_terminator(&block.terminator));
        }
        out
    }
}

#[cfg(test)]
fn fmt_var(v: VarId) -> String {
    format!("v{}", v.0)
}

#[cfg(test)]
fn fmt_instruction(inst: &Instruction) -> String {
    match inst {
        Instruction::Phi { dest, sources } => {
            let srcs: Vec<String> = sources
                .iter()
                .map(|(b, v)| format!("B{}:{}", b.0, fmt_var(*v)))
                .collect();
            format!("{} = Phi({})", fmt_var(*dest), srcs.join(", "))
        }
        Instruction::Copy { dest, src } => {
            format!("{} = Copy({})", fmt_var(*dest), fmt_var(*src))
        }
        Instruction::Const { dest, value } => {
            format!("{} = Const({:?})", fmt_var(*dest), value)
        }
        Instruction::Index { dest, base, key } => {
            format!("{} = {}[{}]", fmt_var(*dest), fmt_var(*base), fmt_var(*key))
        }
        Instruction::Intrinsic { dest, op, args } => {
            let arg_strs: Vec<String> = args.iter().map(|a| fmt_var(*a)).collect();
            format!("{} = {:?}({})", fmt_var(*dest), op, arg_strs.join(", "))
        }
        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let arg_strs: Vec<String> = args.iter().map(|a| fmt_var(*a)).collect();
            format!(
                "{} = Call {}({})",
                fmt_var(*dest),
                function.qualified_name(),
                arg_strs.join(", ")
            )
        }
        Instruction::MakeAccessor { dest, base, key } => {
            format!(
                "{} = MakeAccessor({}[{}])",
                fmt_var(*dest),
                fmt_var(*base),
                fmt_var(*key)
            )
        }
        Instruction::MakeRef { dest, base } => {
            format!("{} = MakeRef({})", fmt_var(*dest), fmt_var(*base))
        }
        Instruction::WriteRef { ref_var, value } => {
            format!("WriteRef({}, {})", fmt_var(*ref_var), fmt_var(*value))
        }
        Instruction::WriteAccessor { base, key, value } => {
            format!(
                "{}[{}] = {}",
                fmt_var(*base),
                fmt_var(*key),
                fmt_var(*value)
            )
        }
        Instruction::Append { dest, arr, value } => {
            format!(
                "{} = Append({}, {})",
                fmt_var(*dest),
                fmt_var(*arr),
                fmt_var(*value)
            )
        }
        Instruction::Reload { dest, src } => {
            format!("{} = Reload({})", fmt_var(*dest), fmt_var(*src))
        }
        Instruction::Assign { slot, value } => {
            format!("Assign(slot_{}, {})", slot, fmt_var(*value))
        }
        Instruction::Read { slot, dest } => {
            format!("{} = Read(slot_{})", fmt_var(*dest), slot)
        }
        Instruction::LoadGlobal { dest, slot } => {
            format!("{} = LoadGlobal(g{})", fmt_var(*dest), slot)
        }
        Instruction::StoreGlobal { slot, value } => {
            format!("StoreGlobal(g{}, {})", slot, fmt_var(*value))
        }
    }
}

#[cfg(test)]
fn fmt_terminator(term: &Terminator) -> String {
    match term {
        Terminator::Jump { target } => format!("Jump B{}", target.0),
        Terminator::If {
            condition,
            then_target,
            else_target,
            ..
        } => {
            format!(
                "If {} → B{}, B{}",
                fmt_var(*condition),
                then_target.0,
                else_target.0
            )
        }
        Terminator::Match {
            value,
            arms,
            default,
            ..
        } => {
            let arm_strs: Vec<String> = arms
                .iter()
                .map(|(pat, target)| format!("{:?}→B{}", pat, target.0))
                .collect();
            format!(
                "Match {} [{}] default→B{}",
                fmt_var(*value),
                arm_strs.join(", "),
                default.0
            )
        }
        Terminator::Return { value } => match value {
            Some(v) => format!("Return {}", fmt_var(*v)),
            None => "Return".to_string(),
        },
        Terminator::Unreachable => "Unreachable".to_string(),
        Terminator::TailCall { args } => {
            let arg_strs: Vec<String> = args.iter().map(|a| fmt_var(*a)).collect();
            format!("TailCall({})", arg_strs.join(", "))
        }
    }
}

/// Complete IR program
#[derive(Debug, Clone)]
pub struct IrProgram {
    pub functions: Vec<Function>,
    /// Number of file-scope global slots (0..N of the VM stack). A synthetic
    /// `__init__` function (present in `functions` when this is non-zero)
    /// initializes them in source order.
    pub global_count: usize,
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
