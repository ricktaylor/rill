//! Builtin Function System
//!
//! Provides a Lua-style registry for builtin functions with metadata that
//! drives compiler lowering decisions.
//!
//! # Purity and Fallibility
//!
//! Each builtin has a purity level that determines optimization potential
//! and whether it may return undefined:
//!
//! - `Impure`: Has side effects, always fallible (may return undefined)
//! - `Pure { fallible }`: No side effects, fallible if domain errors possible
//! - `Const { eval, fallible }`: Can be evaluated at compile time
//!
//! # Example
//!
//! ```ignore
//! let mut registry = BuiltinRegistry::new();
//!
//! registry.register(
//!     BuiltinDef::new("len", builtins::len)
//!         .param("v", TypeSet::collection())
//!         .returns(TypeSet::uint())
//!         .const_eval(const_eval_len)  // Const, fallible by default
//! );
//!
//! registry.register(
//!     BuiltinDef::new("core::make_array", builtins::make_array)
//!         .returns(TypeSet::single(BaseType::Array))
//!         .const_eval_infallible(const_eval_make_array)  // Never fails
//! );
//!
//! registry.register(
//!     BuiltinDef::new("drop", builtins::drop)
//!         .param_optional("reason", TypeSet::uint())
//!         .exits(TypeSet::uint())  // Diverges, implicitly Impure
//! );
//! ```

use super::*;
use exec::{ExecError, Float, HeapVal, VM, Value};
use indexmap::IndexMap;
use ir::{ConstValue, FunctionRef};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use types::{BaseType, TypeSet};

// ============================================================================
// Return Behavior
// ============================================================================

/// Describes how a builtin returns control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnBehavior {
    /// Returns a value of this type to the caller
    /// Whether the return may be undefined is determined by Purity::may_return_undefined()
    Returns(TypeSet),

    /// Never returns to caller - exits to driver with typed value
    /// Lowers to Terminator::Exit
    Exits(TypeSet),
    // Future: Yields(TypeSet) for generators/async
}

impl ReturnBehavior {
    /// Check if this behavior diverges (never returns to caller)
    pub fn diverges(&self) -> bool {
        matches!(self, ReturnBehavior::Exits(_))
    }

    /// Get the type signature (for either Returns or Exits)
    pub fn type_sig(&self) -> &TypeSet {
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
    /// Implicitly fallible - may return undefined due to external factors
    Impure,

    /// No side effects, deterministic given same inputs
    /// Can be reordered, eliminated if unused, and CSE'd
    /// But cannot be evaluated at compile time (may use runtime values)
    /// The bool indicates whether the operation is fallible (can return undefined
    /// for domain reasons like overflow, out-of-bounds, type mismatch)
    Pure { fallible: bool },

    /// Can be evaluated at compile time with the provided evaluator
    /// Valid in const initializers
    /// Implies Pure
    /// The bool indicates whether the operation is fallible (can return undefined
    /// for domain reasons like overflow, out-of-bounds, type mismatch)
    Const { eval: ConstEvalFn, fallible: bool },
}

impl Purity {
    /// Check if this operation may return undefined
    /// - Impure: always true (external factors)
    /// - Pure/Const: depends on the fallible flag
    pub fn may_return_undefined(&self) -> bool {
        match self {
            Purity::Impure => true,
            Purity::Pure { fallible } => *fallible,
            Purity::Const { fallible, .. } => *fallible,
        }
    }

    /// Get the const evaluator if this is a Const purity
    pub fn const_eval(&self) -> Option<ConstEvalFn> {
        match self {
            Purity::Const { eval, .. } => Some(*eval),
            _ => None,
        }
    }

    /// Try to evaluate at compile time
    pub fn try_const_eval(&self, args: &[ConstValue]) -> Option<ConstValue> {
        self.const_eval().and_then(|f| f(args))
    }
}

impl std::fmt::Debug for Purity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Purity::Impure => write!(f, "Impure"),
            Purity::Pure { fallible } => write!(f, "Pure {{ fallible: {} }}", fallible),
            Purity::Const { fallible, .. } => write!(f, "Const {{ fallible: {} }}", fallible),
        }
    }
}

impl PartialEq for Purity {
    fn eq(&self, other: &Self) -> bool {
        // Compare by variant and fallible flag, ignoring the function pointer for Const
        match (self, other) {
            (Purity::Impure, Purity::Impure) => true,
            (Purity::Pure { fallible: a }, Purity::Pure { fallible: b }) => a == b,
            (Purity::Const { fallible: a, .. }, Purity::Const { fallible: b, .. }) => a == b,
            _ => false,
        }
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
                Purity::Pure { .. } => 1,
                Purity::Const { .. } => 2,
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
        matches!(self, Purity::Const { .. })
    }

    /// Check if this purity level allows optimization (reorder, CSE, eliminate)
    pub fn is_pure(&self) -> bool {
        matches!(self, Purity::Pure { .. } | Purity::Const { .. })
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
    pub type_sig: TypeSet,
    /// Whether this parameter is optional
    pub optional: bool,
    /// Whether this parameter is passed by reference
    pub by_ref: bool,
}

impl ParamSpec {
    /// Required parameter
    pub fn required(name: impl Into<String>, type_sig: TypeSet) -> Self {
        ParamSpec {
            name: name.into(),
            type_sig,
            optional: false,
            by_ref: false,
        }
    }

    /// Optional parameter
    pub fn optional(name: impl Into<String>, type_sig: TypeSet) -> Self {
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
    /// Default purity is Pure { fallible: true } (conservative)
    pub fn returning(type_sig: TypeSet) -> Self {
        BuiltinMeta {
            params: Vec::new(),
            returns: ReturnBehavior::Returns(type_sig),
            purity: Purity::Pure { fallible: true },
        }
    }

    /// Create metadata for a function that exits to driver
    pub fn exiting(type_sig: TypeSet) -> Self {
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
    /// Default: returns any type, pure but fallible
    pub fn new(name: impl Into<String>, f: BuiltinFn) -> Self {
        BuiltinDef {
            name: name.into(),
            meta: BuiltinMeta::returning(TypeSet::all()),
            implementation: BuiltinImpl::Native(f),
        }
    }

    /// Create a new builtin definition with a closure
    /// Default: returns any type, pure but fallible
    pub fn with_closure<F>(name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut VM, &[Value]) -> Result<ExecResult, ExecError> + Send + Sync + 'static,
    {
        BuiltinDef {
            name: name.into(),
            meta: BuiltinMeta::returning(TypeSet::all()),
            implementation: BuiltinImpl::Closure(Box::new(f)),
        }
    }

    // Builder methods

    /// Add a required parameter
    pub fn param(mut self, name: impl Into<String>, type_sig: TypeSet) -> Self {
        self.meta.params.push(ParamSpec::required(name, type_sig));
        self
    }

    /// Add an optional parameter
    pub fn param_optional(mut self, name: impl Into<String>, type_sig: TypeSet) -> Self {
        self.meta.params.push(ParamSpec::optional(name, type_sig));
        self
    }

    /// Add a by-reference parameter
    pub fn param_ref(mut self, name: impl Into<String>, type_sig: TypeSet) -> Self {
        self.meta
            .params
            .push(ParamSpec::required(name, type_sig).by_ref());
        self
    }

    /// Set return type (normal return to caller)
    pub fn returns(mut self, type_sig: TypeSet) -> Self {
        self.meta.returns = ReturnBehavior::Returns(type_sig);
        self
    }

    /// Set exit type (diverges, exits to driver)
    pub fn exits(mut self, type_sig: TypeSet) -> Self {
        self.meta.returns = ReturnBehavior::Exits(type_sig);
        self.meta.purity = Purity::Impure; // Exiting is a side effect
        self
    }

    /// Set purity to Const with a const evaluator
    /// Default fallible = true (conservative, can fail for domain reasons)
    pub fn const_eval(mut self, eval: ConstEvalFn) -> Self {
        self.meta.purity = Purity::Const {
            eval,
            fallible: true,
        };
        self
    }

    /// Set purity to Const with infallible operation (never returns undefined)
    /// Use for operations that always succeed: array construction, etc.
    pub fn const_eval_infallible(mut self, eval: ConstEvalFn) -> Self {
        self.meta.purity = Purity::Const {
            eval,
            fallible: false,
        };
        self
    }

    /// Set purity to Pure (no side effects, but can't const eval)
    /// Default fallible = true
    pub fn pure(mut self) -> Self {
        self.meta.purity = Purity::Pure { fallible: true };
        self
    }

    /// Set purity to Pure and infallible
    pub fn pure_infallible(mut self) -> Self {
        self.meta.purity = Purity::Pure { fallible: false };
        self
    }

    /// Set purity to Impure (has side effects, implicitly fallible)
    pub fn impure(mut self) -> Self {
        self.meta.purity = Purity::Impure;
        self
    }

    /// Set purity level (low-level, prefer const_eval/pure/impure helpers)
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

    /// Look up a builtin by function reference
    ///
    /// Uses the qualified name (e.g., "core::add") for lookup.
    pub fn lookup(&self, func: &FunctionRef) -> Option<&BuiltinDef> {
        self.builtins.get(&func.qualified_name())
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
        BuiltinDef::new("core::eq", builtin_eq)
            .param("a", TypeSet::all())
            .param("b", TypeSet::all())
            .returns(TypeSet::bool())
            .const_eval(const_eval_eq),
    );

    registry.register(
        BuiltinDef::new("core::lt", builtin_lt)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::bool())
            .const_eval(const_eval_lt),
    );

    // --- Arithmetic ---
    // Note: These are fallible (overflow possible), which is the default
    registry.register(
        BuiltinDef::new("core::add", builtin_add)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_add),
    );

    registry.register(
        BuiltinDef::new("core::sub", builtin_sub)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_sub),
    );

    registry.register(
        BuiltinDef::new("core::mul", builtin_mul)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_mul),
    );

    registry.register(
        BuiltinDef::new("core::div", builtin_div)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_div),
    );

    registry.register(
        BuiltinDef::new("core::mod", builtin_mod)
            .param("a", TypeSet::numeric())
            .param("b", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_mod),
    );

    registry.register(
        BuiltinDef::new("core::neg", builtin_neg)
            .param("a", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .const_eval(const_eval_neg),
    );

    // --- Logical (non-short-circuit) ---
    registry.register(
        BuiltinDef::new("core::not", builtin_not)
            .param("a", TypeSet::bool())
            .returns(TypeSet::bool())
            .const_eval(const_eval_not),
    );

    // --- Bitwise ---
    registry.register(
        BuiltinDef::new("core::bit_and", builtin_bit_and)
            .param("a", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_bit_and),
    );

    registry.register(
        BuiltinDef::new("core::bit_or", builtin_bit_or)
            .param("a", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_bit_or),
    );

    registry.register(
        BuiltinDef::new("core::bit_xor", builtin_bit_xor)
            .param("a", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_bit_xor),
    );

    registry.register(
        BuiltinDef::new("core::bit_not", builtin_bit_not)
            .param("a", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_bit_not),
    );

    registry.register(
        BuiltinDef::new("core::shl", builtin_shl)
            .param("a", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_shl),
    );

    registry.register(
        BuiltinDef::new("core::shr", builtin_shr)
            .param("a", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::uint())
            .const_eval(const_eval_shr),
    );

    registry.register(
        BuiltinDef::new("core::bit_test", builtin_bit_test)
            .param("x", TypeSet::uint())
            .param("b", TypeSet::uint())
            .returns(TypeSet::bool())
            .const_eval(const_eval_bit_test),
    );

    registry.register(
        BuiltinDef::new("core::bit_set", builtin_bit_set)
            .param("x", TypeSet::uint())
            .param("b", TypeSet::uint())
            .param("v", TypeSet::bool())
            .returns(TypeSet::uint())
            .const_eval(const_eval_bit_set),
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
            .param_optional("reason", TypeSet::uint())
            .exits(TypeSet::uint()),
        // Note: exits() already sets Impure
    );

    registry.register(
        BuiltinDef::new("len", builtin_len)
            .param("value", TypeSet::collection())
            .returns(TypeSet::uint())
            .const_eval(const_eval_len),
    );

    // --- Collection Construction ---
    // These accept any number of arguments (variadic)

    registry.register(
        BuiltinDef::new("core::make_array", builtin_make_array)
            // No fixed params - accepts variadic elements
            .returns(TypeSet::single(BaseType::Array))
            // Array construction always succeeds - infallible
            .const_eval_infallible(const_eval_make_array),
    );

    registry.register(
        BuiltinDef::new("core::make_map", builtin_make_map)
            // No fixed params - accepts variadic key-value pairs (must be even count)
            .returns(TypeSet::single(BaseType::Map))
            // Map construction can fail with odd arg count - fallible (default)
            .const_eval(const_eval_make_map),
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

fn builtin_bit_test(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(x)), Some(Value::UInt(b))) = (args.first(), args.get(1)) {
        // Return undefined if bit position is out of bounds (>= 64)
        if *b >= 64 {
            None
        } else {
            Some(Value::Bool((x >> b) & 1 == 1))
        }
    } else {
        None
    };
    Ok(ExecResult::Return(result))
}

fn builtin_bit_set(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let result = if let (Some(Value::UInt(x)), Some(Value::UInt(b)), Some(Value::Bool(v))) =
        (args.first(), args.get(1), args.get(2))
    {
        // Return undefined if bit position is out of bounds (>= 64)
        if *b >= 64 {
            None
        } else if *v {
            Some(Value::UInt(x | (1 << b)))
        } else {
            Some(Value::UInt(x & !(1 << b)))
        }
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

fn const_eval_bit_test(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(x)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        // Return None (undefined) if bit position is out of bounds (>= 64)
        if *b >= 64 {
            None
        } else {
            Some(ConstValue::Bool((x >> b) & 1 == 1))
        }
    } else {
        None
    }
}

fn const_eval_bit_set(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(x)), Some(ConstValue::UInt(b)), Some(ConstValue::Bool(v))) =
        (args.first(), args.get(1), args.get(2))
    {
        // Return None (undefined) if bit position is out of bounds (>= 64)
        if *b >= 64 {
            None
        } else if *v {
            Some(ConstValue::UInt(x | (1 << b)))
        } else {
            Some(ConstValue::UInt(x & !(1 << b)))
        }
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
        assert!(Purity::Impure < Purity::Pure { fallible: true });
        assert!(
            Purity::Pure { fallible: true }
                < Purity::Const {
                    eval: dummy_const_eval,
                    fallible: true
                }
        );
    }

    #[test]
    fn test_purity_methods() {
        assert!(!Purity::Impure.is_pure());
        assert!(!Purity::Impure.is_const());
        assert!(Purity::Impure.may_return_undefined()); // Impure is always fallible

        let pure = Purity::Pure { fallible: true };
        assert!(pure.is_pure());
        assert!(!pure.is_const());
        assert!(pure.may_return_undefined());

        let pure_infallible = Purity::Pure { fallible: false };
        assert!(!pure_infallible.may_return_undefined());

        let const_purity = Purity::Const {
            eval: dummy_const_eval,
            fallible: true,
        };
        assert!(const_purity.is_pure());
        assert!(const_purity.is_const());
        assert!(const_purity.may_return_undefined());

        let const_infallible = Purity::Const {
            eval: dummy_const_eval,
            fallible: false,
        };
        assert!(!const_infallible.may_return_undefined());
    }

    #[test]
    fn test_const_eval() {
        // Test that const evaluator is callable
        let const_purity = Purity::Const {
            eval: const_eval_len,
            fallible: true,
        };
        assert!(const_purity.const_eval().is_some());

        // Test evaluation with a Text value
        let args = vec![ConstValue::Text("hello".to_string())];
        let result = const_purity.try_const_eval(&args);
        assert_eq!(result, Some(ConstValue::UInt(5)));

        // Test that non-const purity returns None
        assert!(Purity::Pure { fallible: true }.const_eval().is_none());
        assert!(Purity::Impure.const_eval().is_none());
    }

    #[test]
    fn test_return_behavior() {
        let returns = ReturnBehavior::Returns(TypeSet::uint());
        assert!(!returns.diverges());

        let exits = ReturnBehavior::Exits(TypeSet::uint());
        assert!(exits.diverges());
    }

    #[test]
    fn test_builder_pattern() {
        fn dummy(_vm: &mut VM, _args: &[Value]) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(None))
        }

        let def = BuiltinDef::new("test", dummy)
            .param("x", TypeSet::uint())
            .param_optional("y", TypeSet::int())
            .returns(TypeSet::bool())
            .const_eval(dummy_const_eval);

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
