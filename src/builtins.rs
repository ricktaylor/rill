//! Builtin Function System
//!
//! Provides a Lua-style registry for builtin functions with metadata that
//! drives compiler lowering decisions.
//!
//! # Example
//!
//! ```ignore
//! let mut registry = BuiltinRegistry::new();
//!
//! registry.register(
//!     BuiltinDef::new("len", builtins::len)
//!         .param("v", TypeSig::Collection)
//!         .returns(TypeSig::uint())
//!         .purity(Purity::Const(const_eval_len))  // Const with evaluator
//! );
//!
//! registry.register(
//!     BuiltinDef::new("drop", builtins::drop)
//!         .param_optional("reason", TypeSig::UInt)
//!         .exits(TypeSig::uint())
//!         .purity(Purity::Impure)
//! );
//! ```

use super::*;
use exec::{ExecError, Float, HeapVal, VM, Value};
use indexmap::IndexMap;
use ir::ConstValue;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use types::BaseType;

// ============================================================================
// Type Signatures
// ============================================================================

/// Type signature for builtin parameters and return types
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeSig {
    /// Allowed base types
    pub types: Vec<BaseType>,
    /// Whether the value may be missing
    pub maybe_missing: bool,
}

impl TypeSig {
    /// Any single type, not missing
    pub fn of(ty: BaseType) -> Self {
        TypeSig {
            types: vec![ty],
            maybe_missing: false,
        }
    }

    /// UInt type
    pub fn uint() -> Self {
        Self::of(BaseType::UInt)
    }

    /// Int type
    pub fn int() -> Self {
        Self::of(BaseType::Int)
    }

    /// Float type
    pub fn float() -> Self {
        Self::of(BaseType::Float)
    }

    /// Bool type
    pub fn bool() -> Self {
        Self::of(BaseType::Bool)
    }

    /// Text type
    pub fn text() -> Self {
        Self::of(BaseType::Text)
    }

    /// Bytes type
    pub fn bytes() -> Self {
        Self::of(BaseType::Bytes)
    }

    /// Any numeric type (UInt, Int, Float)
    pub fn numeric() -> Self {
        TypeSig {
            types: vec![BaseType::UInt, BaseType::Int, BaseType::Float],
            maybe_missing: false,
        }
    }

    /// Any collection type (Array, Map, Text, Bytes)
    pub fn collection() -> Self {
        TypeSig {
            types: vec![
                BaseType::Array,
                BaseType::Map,
                BaseType::Text,
                BaseType::Bytes,
            ],
            maybe_missing: false,
        }
    }

    /// Any type
    pub fn any() -> Self {
        TypeSig {
            types: vec![
                BaseType::Bool,
                BaseType::UInt,
                BaseType::Int,
                BaseType::Float,
                BaseType::Text,
                BaseType::Bytes,
                BaseType::Array,
                BaseType::Map,
            ],
            maybe_missing: false,
        }
    }

    /// Make this type optional (may be missing)
    pub fn optional(mut self) -> Self {
        self.maybe_missing = true;
        self
    }
}

// ============================================================================
// Return Behavior
// ============================================================================

/// Describes how a builtin returns control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnBehavior {
    /// Returns a value of this type to the caller
    /// The type may have maybe_missing = true
    Returns(TypeSig),

    /// Never returns to caller - exits to driver with typed value
    /// Lowers to Terminator::Exit
    Exits(TypeSig),
    // Future: Yields(TypeSig) for generators/async
}

impl ReturnBehavior {
    /// Check if this behavior diverges (never returns to caller)
    pub fn diverges(&self) -> bool {
        matches!(self, ReturnBehavior::Exits(_))
    }

    /// Get the type signature (for either Returns or Exits)
    pub fn type_sig(&self) -> &TypeSig {
        match self {
            ReturnBehavior::Returns(sig) => sig,
            ReturnBehavior::Exits(sig) => sig,
        }
    }
}

// ============================================================================
// Purity
// ============================================================================

/// Function pointer type for compile-time evaluation of const functions
/// Takes const arguments and returns a const result (or None if evaluation fails)
pub type ConstEvalFn = fn(&[ConstValue]) -> Option<ConstValue>;

/// Purity level of a builtin function
///
/// Forms a hierarchy: Const ⊂ Pure ⊂ Impure
#[derive(Clone, Copy)]
pub enum Purity {
    /// Has side effects or depends on external state
    /// Cannot be reordered, eliminated, or CSE'd
    Impure,

    /// No side effects, deterministic given same inputs
    /// Can be reordered, eliminated if unused, and CSE'd
    /// But cannot be evaluated at compile time (may use runtime values)
    Pure,

    /// Can be evaluated at compile time with the provided evaluator
    /// Valid in const initializers
    /// Implies Pure
    Const(ConstEvalFn),
}

impl std::fmt::Debug for Purity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Purity::Impure => write!(f, "Impure"),
            Purity::Pure => write!(f, "Pure"),
            Purity::Const(_) => write!(f, "Const(fn)"),
        }
    }
}

impl PartialEq for Purity {
    fn eq(&self, other: &Self) -> bool {
        // Compare by variant, ignoring the function pointer for Const
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

impl Eq for Purity {}

impl PartialOrd for Purity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Purity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Ordering: Impure < Pure < Const
        fn rank(p: &Purity) -> u8 {
            match p {
                Purity::Impure => 0,
                Purity::Pure => 1,
                Purity::Const(_) => 2,
            }
        }
        rank(self).cmp(&rank(other))
    }
}

impl Hash for Purity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash by variant discriminant only
        std::mem::discriminant(self).hash(state);
    }
}

impl Purity {
    /// Check if this purity level allows compile-time evaluation
    pub fn is_const(&self) -> bool {
        matches!(self, Purity::Const(_))
    }

    /// Check if this purity level allows optimization (reorder, CSE, eliminate)
    pub fn is_pure(&self) -> bool {
        matches!(self, Purity::Pure | Purity::Const(_))
    }

    /// Get the const evaluator function, if this is a Const purity
    pub fn const_eval(&self) -> Option<ConstEvalFn> {
        match self {
            Purity::Const(f) => Some(*f),
            _ => None,
        }
    }

    /// Evaluate this function at compile time with the given arguments
    /// Returns None if not const or if evaluation fails
    pub fn try_const_eval(&self, args: &[ConstValue]) -> Option<ConstValue> {
        self.const_eval().and_then(|f| f(args))
    }
}

// ============================================================================
// Parameter Specification
// ============================================================================

/// Specification for a builtin parameter
#[derive(Debug, Clone)]
pub struct ParamSpec {
    /// Parameter name (for documentation and error messages)
    pub name: String,
    /// Expected type
    pub type_sig: TypeSig,
    /// Whether this parameter is optional
    pub optional: bool,
    /// Whether this parameter is passed by reference
    pub by_ref: bool,
}

impl ParamSpec {
    /// Required parameter
    pub fn required(name: impl Into<String>, type_sig: TypeSig) -> Self {
        ParamSpec {
            name: name.into(),
            type_sig,
            optional: false,
            by_ref: false,
        }
    }

    /// Optional parameter
    pub fn optional(name: impl Into<String>, type_sig: TypeSig) -> Self {
        ParamSpec {
            name: name.into(),
            type_sig,
            optional: true,
            by_ref: false,
        }
    }

    /// Mark as by-reference parameter
    pub fn by_ref(mut self) -> Self {
        self.by_ref = true;
        self
    }
}

// ============================================================================
// Builtin Metadata
// ============================================================================

/// Metadata for a builtin function, used by the compiler for lowering decisions
#[derive(Debug, Clone)]
pub struct BuiltinMeta {
    /// Parameter specifications
    pub params: Vec<ParamSpec>,
    /// Return behavior (returns or exits)
    pub returns: ReturnBehavior,
    /// Purity level
    pub purity: Purity,
}

impl BuiltinMeta {
    /// Create metadata for a function that returns a value
    pub fn returning(type_sig: TypeSig) -> Self {
        BuiltinMeta {
            params: Vec::new(),
            returns: ReturnBehavior::Returns(type_sig),
            purity: Purity::Pure,
        }
    }

    /// Create metadata for a function that exits to driver
    pub fn exiting(type_sig: TypeSig) -> Self {
        BuiltinMeta {
            params: Vec::new(),
            returns: ReturnBehavior::Exits(type_sig),
            purity: Purity::Impure,
        }
    }

    /// Check if this builtin diverges (never returns to caller)
    pub fn diverges(&self) -> bool {
        self.returns.diverges()
    }

    /// Check if this builtin can be used in const expressions
    pub fn is_const(&self) -> bool {
        self.purity.is_const()
    }

    /// Check if this builtin is pure (can be optimized)
    pub fn is_pure(&self) -> bool {
        self.purity.is_pure()
    }
}

// ============================================================================
// Execution Result
// ============================================================================

/// Result of executing code (builtins, functions, or entire programs)
#[derive(Debug)]
pub enum ExecResult {
    /// Normal return - value goes to caller
    /// None means undefined (operation failed, e.g., overflow, type mismatch)
    Return(Option<Value>),

    /// Hard exit - value goes to driver, never returns to caller
    /// Used by diverging builtins like drop()
    Exit(Value),
}

impl ExecResult {
    /// Create an exit result (for diverging builtins like drop())
    pub fn exit(value: Value) -> Self {
        ExecResult::Exit(value)
    }
}

// ============================================================================
// Builtin Implementation
// ============================================================================

/// Function pointer type for builtin implementations
pub type BuiltinFn = fn(&mut VM, &[Value]) -> Result<ExecResult, ExecError>;

/// Builtin implementation variants
pub enum BuiltinImpl {
    /// Static function pointer
    Native(BuiltinFn),

    /// Boxed closure (for closures capturing state)
    #[allow(clippy::type_complexity)]
    Closure(Box<dyn Fn(&mut VM, &[Value]) -> Result<ExecResult, ExecError> + Send + Sync>),
}

impl std::fmt::Debug for BuiltinImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuiltinImpl::Native(_) => write!(f, "Native(fn)"),
            BuiltinImpl::Closure(_) => write!(f, "Closure(dyn Fn)"),
        }
    }
}

impl BuiltinImpl {
    /// Call the builtin implementation
    pub fn call(&self, vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
        match self {
            BuiltinImpl::Native(f) => f(vm, args),
            BuiltinImpl::Closure(f) => f(vm, args),
        }
    }
}

// ============================================================================
// Builtin Definition
// ============================================================================

/// Complete definition of a builtin function
pub struct BuiltinDef {
    /// Function name
    pub name: String,
    /// Compiler metadata
    pub meta: BuiltinMeta,
    /// Runtime implementation
    pub implementation: BuiltinImpl,
}

impl std::fmt::Debug for BuiltinDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinDef")
            .field("name", &self.name)
            .field("meta", &self.meta)
            .field("implementation", &self.implementation)
            .finish()
    }
}

impl BuiltinDef {
    /// Create a new builtin definition with a native function
    pub fn new(name: impl Into<String>, f: BuiltinFn) -> Self {
        BuiltinDef {
            name: name.into(),
            meta: BuiltinMeta::returning(TypeSig::any().optional()),
            implementation: BuiltinImpl::Native(f),
        }
    }

    /// Create a new builtin definition with a closure
    pub fn with_closure<F>(name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut VM, &[Value]) -> Result<ExecResult, ExecError> + Send + Sync + 'static,
    {
        BuiltinDef {
            name: name.into(),
            meta: BuiltinMeta::returning(TypeSig::any().optional()),
            implementation: BuiltinImpl::Closure(Box::new(f)),
        }
    }

    // Builder methods

    /// Add a required parameter
    pub fn param(mut self, name: impl Into<String>, type_sig: TypeSig) -> Self {
        self.meta.params.push(ParamSpec::required(name, type_sig));
        self
    }

    /// Add an optional parameter
    pub fn param_optional(mut self, name: impl Into<String>, type_sig: TypeSig) -> Self {
        self.meta.params.push(ParamSpec::optional(name, type_sig));
        self
    }

    /// Add a by-reference parameter
    pub fn param_ref(mut self, name: impl Into<String>, type_sig: TypeSig) -> Self {
        self.meta
            .params
            .push(ParamSpec::required(name, type_sig).by_ref());
        self
    }

    /// Set return type (normal return to caller)
    pub fn returns(mut self, type_sig: TypeSig) -> Self {
        self.meta.returns = ReturnBehavior::Returns(type_sig);
        self
    }

    /// Set exit type (diverges, exits to driver)
    pub fn exits(mut self, type_sig: TypeSig) -> Self {
        self.meta.returns = ReturnBehavior::Exits(type_sig);
        self.meta.purity = Purity::Impure; // Exiting is a side effect
        self
    }

    /// Set purity level
    pub fn purity(mut self, purity: Purity) -> Self {
        self.meta.purity = purity;
        self
    }
}

// ============================================================================
// Builtin Registry
// ============================================================================

/// Registry of builtin functions
///
/// Used by the compiler for lowering decisions and by the VM for execution.
#[derive(Debug, Default)]
pub struct BuiltinRegistry {
    builtins: HashMap<String, BuiltinDef>,
}

impl BuiltinRegistry {
    /// Create an empty registry
    pub fn new() -> Self {
        BuiltinRegistry {
            builtins: HashMap::new(),
        }
    }

    /// Register a builtin function
    pub fn register(&mut self, def: BuiltinDef) {
        self.builtins.insert(def.name.clone(), def);
    }

    /// Look up a builtin by name
    pub fn get(&self, name: &str) -> Option<&BuiltinDef> {
        self.builtins.get(name)
    }

    /// Check if a name is a registered builtin
    pub fn contains(&self, name: &str) -> bool {
        self.builtins.contains_key(name)
    }

    /// Iterate over all registered builtins
    pub fn iter(&self) -> impl Iterator<Item = (&String, &BuiltinDef)> {
        self.builtins.iter()
    }

    /// Get the number of registered builtins
    pub fn len(&self) -> usize {
        self.builtins.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.builtins.is_empty()
    }
}

// ============================================================================
// Core Builtins (Operators)
// ============================================================================

/// Register core operator builtins (arithmetic, comparison, bitwise, logical)
/// These correspond to syntax operators: +, -, *, /, %, ==, <, !, &, |, ^, ~, <<, >>
pub fn register_core_builtins(registry: &mut BuiltinRegistry) {
    // --- Comparison ---
    registry.register(
        BuiltinDef::new("core.eq", builtin_eq)
            .param("a", TypeSig::any())
            .param("b", TypeSig::any())
            .returns(TypeSig::bool().optional())
            .purity(Purity::Const(const_eval_eq)),
    );

    registry.register(
        BuiltinDef::new("core.lt", builtin_lt)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::bool().optional())
            .purity(Purity::Const(const_eval_lt)),
    );

    // --- Arithmetic ---
    registry.register(
        BuiltinDef::new("core.add", builtin_add)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_add)),
    );

    registry.register(
        BuiltinDef::new("core.sub", builtin_sub)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_sub)),
    );

    registry.register(
        BuiltinDef::new("core.mul", builtin_mul)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_mul)),
    );

    registry.register(
        BuiltinDef::new("core.div", builtin_div)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_div)),
    );

    registry.register(
        BuiltinDef::new("core.mod", builtin_mod)
            .param("a", TypeSig::numeric())
            .param("b", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_mod)),
    );

    registry.register(
        BuiltinDef::new("core.neg", builtin_neg)
            .param("a", TypeSig::numeric())
            .returns(TypeSig::numeric().optional())
            .purity(Purity::Const(const_eval_neg)),
    );

    // --- Logical (non-short-circuit) ---
    registry.register(
        BuiltinDef::new("core.not", builtin_not)
            .param("a", TypeSig::bool())
            .returns(TypeSig::bool().optional())
            .purity(Purity::Const(const_eval_not)),
    );

    // --- Bitwise ---
    registry.register(
        BuiltinDef::new("core.bit_and", builtin_bit_and)
            .param("a", TypeSig::uint())
            .param("b", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_bit_and)),
    );

    registry.register(
        BuiltinDef::new("core.bit_or", builtin_bit_or)
            .param("a", TypeSig::uint())
            .param("b", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_bit_or)),
    );

    registry.register(
        BuiltinDef::new("core.bit_xor", builtin_bit_xor)
            .param("a", TypeSig::uint())
            .param("b", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_bit_xor)),
    );

    registry.register(
        BuiltinDef::new("core.bit_not", builtin_bit_not)
            .param("a", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_bit_not)),
    );

    registry.register(
        BuiltinDef::new("core.shl", builtin_shl)
            .param("a", TypeSig::uint())
            .param("b", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_shl)),
    );

    registry.register(
        BuiltinDef::new("core.shr", builtin_shr)
            .param("a", TypeSig::uint())
            .param("b", TypeSig::uint())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_shr)),
    );
}

// ============================================================================
// Standard Builtins
// ============================================================================

/// Create a registry with standard builtins (includes core)
pub fn standard_builtins() -> BuiltinRegistry {
    let mut registry = BuiltinRegistry::new();

    // Register core operator builtins
    register_core_builtins(&mut registry);

    // Exit/control flow
    registry.register(
        BuiltinDef::new("drop", builtin_drop)
            .param_optional("reason", TypeSig::uint())
            .exits(TypeSig::uint())
            .purity(Purity::Impure),
    );

    registry.register(
        BuiltinDef::new("len", builtin_len)
            .param("value", TypeSig::collection())
            .returns(TypeSig::uint().optional())
            .purity(Purity::Const(const_eval_len)),
    );

    // --- Collection Construction ---
    // These accept any number of arguments (variadic)

    registry.register(
        BuiltinDef::new("core.make_array", builtin_make_array)
            // No fixed params - accepts variadic elements
            .returns(TypeSig::of(BaseType::Array))
            .purity(Purity::Const(const_eval_make_array)),
    );

    registry.register(
        BuiltinDef::new("core.make_map", builtin_make_map)
            // No fixed params - accepts variadic key-value pairs (must be even count)
            .returns(TypeSig::of(BaseType::Map).optional()) // Optional: fails if odd arg count
            .purity(Purity::Const(const_eval_make_map)),
    );

    registry
}

// ============================================================================
// Builtin Implementations
// ============================================================================

fn builtin_drop(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let reason = args.first().cloned().unwrap_or(Value::UInt(0));
    Ok(ExecResult::exit(reason))
}

fn builtin_len(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let Some(value) = args.first() {
        match value {
            Value::Text(s) => Some(s.chars().count() as u64),
            Value::Bytes(b) => Some(b.len() as u64),
            Value::Array(arr) => Some(arr.len() as u64),
            Value::Map(map) => Some(map.len() as u64),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result.map(Value::UInt)))
}

// ============================================================================
// Core Builtin Implementations
// ============================================================================

// --- Comparison ---

fn builtin_eq(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        Some(Value::Bool(a == b))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_lt(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            (Value::UInt(a), Value::UInt(b)) => Some(a < b),
            (Value::Int(a), Value::Int(b)) => Some(a < b),
            (Value::Float(a), Value::Float(b)) => Some(a.get() < b.get()),
            (Value::UInt(a), Value::Int(b)) => Some((*a as i128) < (*b as i128)),
            (Value::Int(a), Value::UInt(b)) => Some((*a as i128) < (*b as i128)),
            (Value::UInt(a), Value::Float(b)) => Some((*a as f64) < b.get()),
            (Value::Float(a), Value::UInt(b)) => Some(a.get() < (*b as f64)),
            (Value::Int(a), Value::Float(b)) => Some((*a as f64) < b.get()),
            (Value::Float(a), Value::Int(b)) => Some(a.get() < (*b as f64)),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result.map(Value::Bool)))
}

// --- Arithmetic ---
// All integer operations use checked arithmetic - overflow/underflow returns Undefined.
// Float operations use standard IEEE semantics (can produce Inf/NaN).

fn builtin_add(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            (Value::UInt(a), Value::UInt(b)) => a.checked_add(*b).map(Value::UInt),
            (Value::Int(a), Value::Int(b)) => a.checked_add(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Float::new(a.get() + b.get()).map(Value::Float),
            // Mixed integer types: promote to Int, check for overflow
            (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
                .ok()
                .and_then(|a| a.checked_add(*b))
                .map(Value::Int),
            (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
                .ok()
                .and_then(|b| a.checked_add(b))
                .map(Value::Int),
            // Float promotion (no overflow check needed)
            (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
            (Value::Float(a), Value::UInt(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
            (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
            (Value::Float(a), Value::Int(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_sub(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            (Value::UInt(a), Value::UInt(b)) => a.checked_sub(*b).map(Value::UInt),
            (Value::Int(a), Value::Int(b)) => a.checked_sub(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Float::new(a.get() - b.get()).map(Value::Float),
            // Mixed integer types: promote to Int, check for overflow
            (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
                .ok()
                .and_then(|a| a.checked_sub(*b))
                .map(Value::Int),
            (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
                .ok()
                .and_then(|b| a.checked_sub(b))
                .map(Value::Int),
            // Float promotion (no overflow check needed)
            (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
            (Value::Float(a), Value::UInt(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
            (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
            (Value::Float(a), Value::Int(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_mul(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            (Value::UInt(a), Value::UInt(b)) => a.checked_mul(*b).map(Value::UInt),
            (Value::Int(a), Value::Int(b)) => a.checked_mul(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Float::new(a.get() * b.get()).map(Value::Float),
            // Mixed integer types: promote to Int, check for overflow
            (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
                .ok()
                .and_then(|a| a.checked_mul(*b))
                .map(Value::Int),
            (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
                .ok()
                .and_then(|b| a.checked_mul(b))
                .map(Value::Int),
            // Float promotion (no overflow check needed)
            (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
            (Value::Float(a), Value::UInt(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
            (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
            (Value::Float(a), Value::Int(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_div(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            // checked_div handles both divide-by-zero and overflow (MIN / -1)
            (Value::UInt(a), Value::UInt(b)) => a.checked_div(*b).map(Value::UInt),
            (Value::Int(a), Value::Int(b)) => a.checked_div(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Float::new(a.get() / b.get()).map(Value::Float),
            // Mixed integer types
            (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
                .ok()
                .and_then(|a| a.checked_div(*b))
                .map(Value::Int),
            (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
                .ok()
                .and_then(|b| a.checked_div(b))
                .map(Value::Int),
            // Float promotion
            (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
            (Value::Float(a), Value::UInt(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
            (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
            (Value::Float(a), Value::Int(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_mod(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(a), Some(b)) = (args.first(), args.get(1)) {
        match (a, b) {
            // checked_rem handles divide-by-zero and overflow
            (Value::UInt(a), Value::UInt(b)) => a.checked_rem(*b).map(Value::UInt),
            (Value::Int(a), Value::Int(b)) => a.checked_rem(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Float::new(a.get() % b.get()).map(Value::Float),
            // Mixed integer types
            (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
                .ok()
                .and_then(|a| a.checked_rem(*b))
                .map(Value::Int),
            (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
                .ok()
                .and_then(|b| a.checked_rem(b))
                .map(Value::Int),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_neg(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let Some(a) = args.first() {
        match a {
            // checked_neg handles MIN overflow
            Value::Int(a) => a.checked_neg().map(Value::Int),
            Value::Float(a) => Float::new(-a.get()).map(Value::Float),
            // UInt negation: convert to Int first, then negate
            Value::UInt(a) => i64::try_from(*a)
                .ok()
                .and_then(|v| v.checked_neg())
                .map(Value::Int),
            _ => None,
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

// --- Logical ---

fn builtin_not(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let Some(Value::Bool(a)) = args.first() {
        Some(Value::Bool(!*a))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

// --- Bitwise ---

fn builtin_bit_and(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(a)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        Some(Value::UInt(a & b))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_bit_or(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(a)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        Some(Value::UInt(a | b))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_bit_xor(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(a)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        Some(Value::UInt(a ^ b))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_bit_not(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let Some(Value::UInt(a)) = args.first() {
        Some(Value::UInt(!a))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_shl(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(a)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        Some(Value::UInt(a.wrapping_shl(*b as u32)))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_shr(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(a)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        Some(Value::UInt(a.wrapping_shr(*b as u32)))
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

// ============================================================================
// Const Evaluators (compile-time evaluation)
// ============================================================================

/// Const evaluator for len - returns length of collections
fn const_eval_len(args: &[ConstValue]) -> Option<ConstValue> {
    let len = match args.first()? {
        ConstValue::Text(s) => s.chars().count() as u64,
        ConstValue::Bytes(b) => b.len() as u64,
        ConstValue::Array(arr) => arr.len() as u64,
        ConstValue::Map(map) => map.len() as u64,
        _ => return None, // Type mismatch - can't evaluate
    };
    Some(ConstValue::UInt(len))
}

// ============================================================================
// Core Const Evaluators
// ============================================================================

// --- Comparison ---

fn const_eval_eq(args: &[ConstValue]) -> Option<ConstValue> {
    Some(ConstValue::Bool(args.first()? == args.get(1)?))
}

fn const_eval_lt(args: &[ConstValue]) -> Option<ConstValue> {
    let result = match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a < b,
        (ConstValue::Int(a), ConstValue::Int(b)) => a < b,
        (ConstValue::Float(a), ConstValue::Float(b)) => a < b,
        (ConstValue::UInt(a), ConstValue::Int(b)) => (*a as i128) < (*b as i128),
        (ConstValue::Int(a), ConstValue::UInt(b)) => (*a as i128) < (*b as i128),
        (ConstValue::UInt(a), ConstValue::Float(b)) => (*a as f64) < *b,
        (ConstValue::Float(a), ConstValue::UInt(b)) => *a < (*b as f64),
        (ConstValue::Int(a), ConstValue::Float(b)) => (*a as f64) < *b,
        (ConstValue::Float(a), ConstValue::Int(b)) => *a < (*b as f64),
        _ => return None,
    };
    Some(ConstValue::Bool(result))
}

// --- Arithmetic ---
// All integer operations use checked arithmetic - overflow/underflow returns None (Undefined).

fn const_eval_add(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_add(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_add(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a + b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_add(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_add(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 + b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a + *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 + b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a + *b as f64)),
        _ => None,
    }
}

fn const_eval_sub(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_sub(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_sub(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a - b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_sub(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_sub(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 - b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a - *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 - b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a - *b as f64)),
        _ => None,
    }
}

fn const_eval_mul(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_mul(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_mul(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a * b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_mul(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_mul(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 * b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a * *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 * b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a * *b as f64)),
        _ => None,
    }
}

fn const_eval_div(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_div(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_div(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a / b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_div(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_div(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 / b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a / *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 / b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a / *b as f64)),
        _ => None,
    }
}

fn const_eval_mod(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_rem(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_rem(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a % b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_rem(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_rem(b))
            .map(ConstValue::Int),
        _ => None,
    }
}

fn const_eval_neg(args: &[ConstValue]) -> Option<ConstValue> {
    match args.first()? {
        ConstValue::Int(a) => a.checked_neg().map(ConstValue::Int),
        ConstValue::Float(a) => Some(ConstValue::Float(-a)),
        ConstValue::UInt(a) => i64::try_from(*a)
            .ok()
            .and_then(|v| v.checked_neg())
            .map(ConstValue::Int),
        _ => None,
    }
}

// --- Logical ---

fn const_eval_not(args: &[ConstValue]) -> Option<ConstValue> {
    if let Some(ConstValue::Bool(a)) = args.first() {
        Some(ConstValue::Bool(!a))
    } else {
        None
    }
}

// --- Bitwise ---

fn const_eval_bit_and(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a & b))
    } else {
        None
    }
}

fn const_eval_bit_or(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a | b))
    } else {
        None
    }
}

fn const_eval_bit_xor(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a ^ b))
    } else {
        None
    }
}

fn const_eval_bit_not(args: &[ConstValue]) -> Option<ConstValue> {
    if let Some(ConstValue::UInt(a)) = args.first() {
        Some(ConstValue::UInt(!a))
    } else {
        None
    }
}

fn const_eval_shl(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a.wrapping_shl(*b as u32)))
    } else {
        None
    }
}

fn const_eval_shr(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a.wrapping_shr(*b as u32)))
    } else {
        None
    }
}

// --- Collection Construction ---

fn builtin_make_array(vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let arr = HeapVal::new(args.to_vec(), vm.heap())?;
    Ok(ExecResult::Return(Some(Value::Array(arr))))
}

fn const_eval_make_array(args: &[ConstValue]) -> Option<ConstValue> {
    Some(ConstValue::Array(args.to_vec()))
}

fn builtin_make_map(vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    // Args must be key-value pairs (even count)
    if !args.len().is_multiple_of(2) {
        return Ok(ExecResult::Return(None));
    }
    let map: IndexMap<Value, Value> = args
        .chunks(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect();
    let heap_map = HeapVal::new(map, vm.heap())?;
    Ok(ExecResult::Return(Some(Value::Map(heap_map))))
}

fn const_eval_make_map(args: &[ConstValue]) -> Option<ConstValue> {
    // Args must be key-value pairs (even count)
    if !args.len().is_multiple_of(2) {
        return None;
    }
    let pairs: Vec<(ConstValue, ConstValue)> = args
        .chunks(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect();
    Some(ConstValue::Map(pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dummy const evaluator for tests
    fn dummy_const_eval(_args: &[ConstValue]) -> Option<ConstValue> {
        Some(ConstValue::Bool(true))
    }

    #[test]
    fn test_purity_ordering() {
        assert!(Purity::Impure < Purity::Pure);
        assert!(Purity::Pure < Purity::Const(dummy_const_eval));
    }

    #[test]
    fn test_purity_methods() {
        assert!(!Purity::Impure.is_pure());
        assert!(!Purity::Impure.is_const());

        assert!(Purity::Pure.is_pure());
        assert!(!Purity::Pure.is_const());

        let const_purity = Purity::Const(dummy_const_eval);
        assert!(const_purity.is_pure());
        assert!(const_purity.is_const());
    }

    #[test]
    fn test_const_eval() {
        // Test that const evaluator is callable
        let const_purity = Purity::Const(const_eval_len);
        assert!(const_purity.const_eval().is_some());

        // Test evaluation with a Text value
        let args = vec![ConstValue::Text("hello".to_string())];
        let result = const_purity.try_const_eval(&args);
        assert_eq!(result, Some(ConstValue::UInt(5)));

        // Test that non-const purity returns None
        assert!(Purity::Pure.const_eval().is_none());
        assert!(Purity::Impure.const_eval().is_none());
    }

    #[test]
    fn test_return_behavior() {
        let returns = ReturnBehavior::Returns(TypeSig::uint());
        assert!(!returns.diverges());

        let exits = ReturnBehavior::Exits(TypeSig::uint());
        assert!(exits.diverges());
    }

    #[test]
    fn test_builder_pattern() {
        fn dummy(_vm: &mut VM, _args: &[Value]) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(None))
        }

        let def = BuiltinDef::new("test", dummy)
            .param("x", TypeSig::uint())
            .param_optional("y", TypeSig::int())
            .returns(TypeSig::bool())
            .purity(Purity::Const(dummy_const_eval));

        assert_eq!(def.name, "test");
        assert_eq!(def.meta.params.len(), 2);
        assert!(!def.meta.params[0].optional);
        assert!(def.meta.params[1].optional);
        assert!(def.meta.is_const());
        assert!(!def.meta.diverges());
    }

    #[test]
    fn test_registry() {
        fn dummy(_vm: &mut VM, _args: &[Value]) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(None))
        }

        let mut registry = BuiltinRegistry::new();
        assert!(registry.is_empty());

        registry.register(BuiltinDef::new("foo", dummy));
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("foo"));
        assert!(!registry.contains("bar"));

        let def = registry.get("foo").unwrap();
        assert_eq!(def.name, "foo");
    }
}
