//! Extern Function System
//!
//! Provides a registry for host-provided extern functions that Rill scripts
//! can call by name. This is the embedding API — how the host application
//! (and optional stdlib crate) extends the language with custom capabilities.
//!
//! Language-defined operators (arithmetic, comparison, bitwise, etc.) and
//! compiler-internal synthetics (array/map construction, etc.) are handled
//! as `IntrinsicOp` (core intrinsics) and do not appear in this registry.
//!
//! # Purity and Fallibility
//!
//! Each extern has a purity level that determines optimization potential
//! and whether it may return undefined:
//!
//! - `Impure`: Has side effects, always fallible (may return undefined)
//! - `Pure { fallible }`: No side effects, fallible if domain errors possible
//! - `Const { eval, fallible }`: Can be evaluated at compile time
//!
//! # Example
//!
//! ```ignore
//! let mut registry = ExternRegistry::new();
//!
//! registry.register(
//!     ExternDef::new("runtime", "exit", my_exit_impl)
//!         .param_optional("code", TypeSet::uint())
//!         .exits(TypeSet::uint())
//! )?;
//!
//! registry.register(
//!     ExternDef::new("cbor", "decode", my_decode_impl)
//!         .param("data", TypeSet::bytes())
//!         .returns(TypeSet::any())
//!         .pure()
//! )?;
//! ```
//!
//! Scripts access these via `require`:
//! ```rill
//! require runtime as _;  // exit() available unqualified
//! require cbor;           // cbor::decode() available qualified
//! ```

use super::*;
use exec::{ExecError, VM, Value};
use ir::{ConstValue, FunctionRef};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use types::TypeSet;

// ============================================================================
// Return Behavior
// ============================================================================

/// Describes how an extern returns control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnBehavior {
    /// Returns a value of this type to the caller
    /// Whether the return may be undefined is determined by Purity::may_return_undefined()
    Returns(TypeSet),

    /// Never returns to caller - exits to driver with typed value.
    /// At runtime, the extern returns ExecResult::Exit(val) which propagates
    /// up through all call frames via Action::Exit.
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

/// Purity level of an extern function
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

    /// Check if this purity level allows compile-time evaluation
    pub fn is_const(&self) -> bool {
        matches!(self, Purity::Const { .. })
    }

    /// Check if this purity level allows optimization (reorder, CSE, eliminate)
    pub fn is_pure(&self) -> bool {
        matches!(self, Purity::Pure { .. } | Purity::Const { .. })
    }
}

impl core::fmt::Debug for Purity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Purity::Impure => write!(f, "Impure"),
            Purity::Pure { fallible } => write!(f, "Pure {{ fallible: {} }}", fallible),
            Purity::Const { fallible, .. } => write!(f, "Const {{ fallible: {} }}", fallible),
        }
    }
}

impl PartialEq for Purity {
    fn eq(&self, other: &Self) -> bool {
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
        std::mem::discriminant(self).hash(state);
    }
}

// ============================================================================
// Parameter Specification
// ============================================================================

/// Specification for an extern parameter
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
// Extern Metadata
// ============================================================================

/// Metadata for an extern function, used by the compiler for lowering decisions
#[derive(Debug, Clone)]
pub struct ExternMeta {
    /// Parameter specifications
    pub params: Vec<ParamSpec>,
    /// Return behavior (returns or exits)
    pub returns: ReturnBehavior,
    /// Purity level
    pub purity: Purity,
}

impl ExternMeta {
    /// Create metadata for a function that returns a value
    /// Default purity is Pure { fallible: true } (conservative)
    pub fn returning(type_sig: TypeSet) -> Self {
        ExternMeta {
            params: Vec::new(),
            returns: ReturnBehavior::Returns(type_sig),
            purity: Purity::Pure { fallible: true },
        }
    }

    /// Create metadata for a function that exits to driver
    pub fn exiting(type_sig: TypeSet) -> Self {
        ExternMeta {
            params: Vec::new(),
            returns: ReturnBehavior::Exits(type_sig),
            purity: Purity::Impure,
        }
    }

    /// Check if this extern diverges (never returns to caller)
    pub fn diverges(&self) -> bool {
        self.returns.diverges()
    }

    /// Check if this extern can be used in const expressions
    pub fn is_const(&self) -> bool {
        self.purity.is_const()
    }

    /// Check if this extern is pure (can be optimized)
    pub fn is_pure(&self) -> bool {
        self.purity.is_pure()
    }
}

// ============================================================================
// Execution Result
// ============================================================================

/// Result of executing code (externs, functions, or entire programs)
#[derive(Debug)]
pub enum ExecResult {
    /// Normal return - value goes to caller.
    /// `Value::Undefined` means the operation failed (overflow, type mismatch, etc.)
    Return(Value),

    /// Hard exit - value goes to driver, never returns to caller
    /// Used by diverging externs like exit()
    Exit(Value),
}

impl ExecResult {
    /// Create an exit result (for diverging externs like exit())
    pub fn exit(value: Value) -> Self {
        ExecResult::Exit(value)
    }
}

// ============================================================================
// Extern Implementation
// ============================================================================

/// Function pointer type for extern implementations.
///
/// Externs use a frame-based calling convention (inspired by Lua's C API):
/// arguments are placed in the current stack frame at slots 1..=N.
/// The `usize` parameter is the argument count.
///
/// Access args via `vm.arg(i)`:
/// ```ignore
/// fn my_extern(vm: &mut VM, argc: usize) -> Result<ExecResult, ExecError> {
///     let x = vm.arg(0).clone();
///     Ok(ExecResult::Return(x))
/// }
/// ```
pub type ExternFn = fn(&mut VM, usize) -> Result<ExecResult, ExecError>;

/// Extern implementation variants
pub enum ExternImpl {
    /// Static function pointer
    Native(ExternFn),

    /// Boxed closure (for closures capturing state)
    #[allow(clippy::type_complexity)]
    Closure(Box<dyn Fn(&mut VM, usize) -> Result<ExecResult, ExecError> + Send + Sync>),
}

impl core::fmt::Debug for ExternImpl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExternImpl::Native(_) => write!(f, "Native(fn)"),
            ExternImpl::Closure(_) => write!(f, "Closure(dyn Fn)"),
        }
    }
}

impl ExternImpl {
    /// Call the extern implementation
    pub fn call(&self, vm: &mut VM, argc: usize) -> Result<ExecResult, ExecError> {
        match self {
            ExternImpl::Native(f) => f(vm, argc),
            ExternImpl::Closure(f) => f(vm, argc),
        }
    }
}

// ============================================================================
// Extern Definition
// ============================================================================

/// A type-specialized variant of an extern function.
///
/// When the compiler proves all arguments match the variant's param types
/// at compile time, it selects this variant instead of the generic implementation.
/// The variant's param guards are tighter, so the optimizer eliminates them.
pub struct ExternVariant {
    /// Required param types for this variant (positional, must match exactly)
    pub param_types: Vec<TypeSet>,
    /// Return type for this variant (may be tighter than the generic)
    pub returns: TypeSet,
    /// Type-specialized implementation
    pub implementation: ExternImpl,
}

impl core::fmt::Debug for ExternVariant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternVariant")
            .field("param_types", &self.param_types)
            .field("returns", &self.returns)
            .finish()
    }
}

/// Complete definition of an extern function
pub struct ExternDef {
    /// Namespace (e.g., "math", "cbor")
    pub namespace: String,
    /// Function name
    pub name: String,
    /// Compiler metadata
    pub meta: ExternMeta,
    /// Runtime implementation (generic fallback)
    pub implementation: ExternImpl,
    /// Type-specialized variants (selected when arg types are statically known)
    pub variants: Vec<ExternVariant>,
}

impl core::fmt::Debug for ExternDef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternDef")
            .field("namespace", &self.namespace)
            .field("name", &self.name)
            .field("meta", &self.meta)
            .field("implementation", &self.implementation)
            .field("variants", &self.variants)
            .finish()
    }
}

impl ExternDef {
    /// Create a new extern definition with a native function.
    ///
    /// `namespace` groups the function — scripts use `require namespace;`
    /// to access it as `namespace::name()`.
    ///
    /// Default: returns any type, pure but fallible.
    pub fn new(namespace: impl Into<String>, name: impl Into<String>, f: ExternFn) -> Self {
        ExternDef {
            namespace: namespace.into(),
            name: name.into(),
            meta: ExternMeta::returning(TypeSet::any()),
            implementation: ExternImpl::Native(f),
            variants: Vec::new(),
        }
    }

    /// Create a new extern definition with a closure.
    ///
    /// Default: returns any type, pure but fallible.
    pub fn with_closure<F>(namespace: impl Into<String>, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut VM, usize) -> Result<ExecResult, ExecError> + Send + Sync + 'static,
    {
        ExternDef {
            namespace: namespace.into(),
            name: name.into(),
            meta: ExternMeta::returning(TypeSet::any()),
            implementation: ExternImpl::Closure(Box::new(f)),
            variants: Vec::new(),
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

    /// Add a type-specialized variant with a native function.
    ///
    /// When the compiler proves all arguments match `param_types` at compile
    /// time, this variant is selected instead of the generic implementation.
    ///
    /// ```ignore
    /// ExternDef::new("math", "sqrt", sqrt_generic)
    ///     .param("x", TypeSet::numeric())
    ///     .returns(TypeSet::numeric())
    ///     .variant(&[TypeSet::uint()], TypeSet::uint(), sqrt_uint)
    ///     .variant(&[TypeSet::single(Float)], TypeSet::single(Float), sqrt_float)
    /// ```
    pub fn variant(mut self, param_types: &[TypeSet], returns: TypeSet, f: ExternFn) -> Self {
        self.variants.push(ExternVariant {
            param_types: param_types.to_vec(),
            returns,
            implementation: ExternImpl::Native(f),
        });
        self
    }

    /// Add a type-specialized variant with a closure.
    pub fn variant_closure<F>(mut self, param_types: &[TypeSet], returns: TypeSet, f: F) -> Self
    where
        F: Fn(&mut VM, usize) -> Result<ExecResult, ExecError> + Send + Sync + 'static,
    {
        self.variants.push(ExternVariant {
            param_types: param_types.to_vec(),
            returns,
            implementation: ExternImpl::Closure(Box::new(f)),
        });
        self
    }

    /// Find the best matching variant for the given argument types.
    ///
    /// Returns the variant whose param_types all match (each arg TypeSet is a
    /// subset of or equal to the variant's param TypeSet). Returns None if no
    /// variant matches or arg types are too broad.
    pub fn select_variant(&self, arg_types: &[TypeSet]) -> Option<&ExternVariant> {
        self.variants.iter().find(|v| {
            v.param_types.len() == arg_types.len()
                && v.param_types
                    .iter()
                    .zip(arg_types)
                    .all(|(spec, actual)| !actual.is_empty() && actual.difference(spec).is_empty())
        })
    }
}

// ============================================================================
// Extern Registry
// ============================================================================

/// Error returned when extern registration fails.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Function name was already registered in this namespace.
    #[error("extern `{namespace}::{name}` is already registered")]
    DuplicateInNamespace { namespace: String, name: String },
}

/// Registry of extern functions
///
/// Contains embedder-provided functions that Rill scripts can call.
/// All externs are namespaced — scripts use `require namespace;` to
/// access them as `namespace::func()`, or `require namespace as _;`
/// to merge into root scope.
///
/// Language-defined operators are handled as `IntrinsicOp` (core
/// intrinsics) and do not appear here.
#[derive(Debug, Default)]
pub struct ExternRegistry {
    /// Namespaced externs — namespace → (function name → def)
    namespaces: HashMap<String, HashMap<String, ExternDef>>,
}

impl ExternRegistry {
    /// Create an empty registry
    pub fn new() -> Self {
        ExternRegistry {
            namespaces: HashMap::new(),
        }
    }

    /// Register an extern function.
    ///
    /// The namespace and name come from the `ExternDef`.
    /// Scripts use `require namespace;` to access as `namespace::func()`,
    /// or `require namespace as _;` to merge into root scope.
    ///
    /// ```ignore
    /// registry.register(ExternDef::new("cbor", "decode", decode_fn))?;
    /// registry.register(ExternDef::new("cbor", "encode", encode_fn))?;
    /// ```
    pub fn register(&mut self, def: ExternDef) -> Result<(), RegistryError> {
        let ns = self.namespaces.entry(def.namespace.clone()).or_default();
        if ns.contains_key(&def.name) {
            return Err(RegistryError::DuplicateInNamespace {
                namespace: def.namespace.clone(),
                name: def.name.clone(),
            });
        }
        ns.insert(def.name.clone(), def);
        Ok(())
    }

    /// Look up a namespaced extern by namespace and function name.
    pub fn get_in(&self, namespace: &str, name: &str) -> Option<&ExternDef> {
        self.namespaces.get(namespace)?.get(name)
    }

    /// Look up an extern by function reference.
    ///
    /// Only resolves qualified (namespaced) references.
    pub fn lookup(&self, func: &FunctionRef) -> Option<&ExternDef> {
        let ns = func.namespace.as_ref()?;
        self.get_in(ns, &func.name)
    }

    /// Check if a namespace has been registered.
    pub fn has_namespace(&self, namespace: &str) -> bool {
        self.namespaces.contains_key(namespace)
    }

    /// Get the names of all registered namespaces.
    pub fn namespace_names(&self) -> impl Iterator<Item = &str> {
        self.namespaces.keys().map(|s| s.as_str())
    }

    /// Iterate over all externs in a namespace.
    pub fn namespace_iter(&self, namespace: &str) -> impl Iterator<Item = (&String, &ExternDef)> {
        self.namespaces
            .get(namespace)
            .into_iter()
            .flat_map(|ns| ns.iter())
    }

    /// Iterate over all registered externs.
    ///
    /// Yields `(qualified_name, def)` where qualified_name is `"ns::func"`.
    pub fn iter(&self) -> impl Iterator<Item = (String, &ExternDef)> {
        self.namespaces.iter().flat_map(|(ns, funcs)| {
            funcs
                .iter()
                .map(move |(name, def)| (format!("{ns}::{name}"), def))
        })
    }

    /// Get the total number of registered externs.
    pub fn len(&self) -> usize {
        self.namespaces.values().map(|ns| ns.len()).sum::<usize>()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }
}

// ============================================================================
// Standard Externs
// ============================================================================

/// Create a registry with standard externs.
///
/// Currently empty — all language-defined operations are core intrinsics.
/// The stdlib crate (when implemented) will register its functions here.
pub fn standard_externs() -> ExternRegistry {
    // Only `len` is a compiler intrinsic (via try_lower_intrinsic).
    // Type-checking (is_uint, etc.) and presence-checking (is_some)
    // are user-definable — will move to a prelude in the future.
    // This registry is reserved for host-provided extern functions.
    //
    // Future: exit, encode, decode, print, etc.

    ExternRegistry::new()
}

// ============================================================================
// Tests
// ============================================================================

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
            eval: dummy_const_eval,
            fallible: true,
        };
        assert!(const_purity.const_eval().is_some());

        // Test evaluation
        let result = const_purity.try_const_eval(&[]);
        assert_eq!(result, Some(ConstValue::Bool(true)));

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
        fn dummy(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::Undefined))
        }

        let def = ExternDef::new("ns", "test", dummy)
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
        fn dummy(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::Undefined))
        }

        let mut registry = ExternRegistry::new();
        assert!(registry.is_empty());

        registry
            .register(ExternDef::new("test", "foo", dummy))
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.has_namespace("test"));

        let def = registry.get_in("test", "foo").unwrap();
        assert_eq!(def.name, "foo");
    }

    #[test]
    fn test_register_no_intrinsic_clash_when_namespaced() {
        fn dummy(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::Undefined))
        }

        // Intrinsic names are fine in namespaces — clash only happens at `require ns as _`
        let mut registry = ExternRegistry::new();
        registry
            .register(ExternDef::new("core", "len", dummy))
            .unwrap();
        registry
            .register(ExternDef::new("core", "collect", dummy))
            .unwrap();
    }

    #[test]
    fn test_register_duplicate_in_namespace() {
        fn dummy(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::Undefined))
        }

        let mut registry = ExternRegistry::new();
        registry
            .register(ExternDef::new("cbor", "decode", dummy))
            .unwrap();

        let result = registry.register(ExternDef::new("cbor", "decode", dummy));
        assert!(matches!(
            result,
            Err(RegistryError::DuplicateInNamespace { .. })
        ));

        // Same name in different namespace is fine
        registry
            .register(ExternDef::new("json", "decode", dummy))
            .unwrap();
    }

    #[test]
    fn test_namespaced_lookup() {
        fn dummy(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::Undefined))
        }

        let mut registry = ExternRegistry::new();
        registry
            .register(ExternDef::new("cbor", "decode", dummy))
            .unwrap();
        registry
            .register(ExternDef::new("cbor", "encode", dummy))
            .unwrap();

        assert!(registry.has_namespace("cbor"));
        assert!(!registry.has_namespace("json"));

        assert!(registry.get_in("cbor", "decode").is_some());
        assert!(registry.get_in("cbor", "encode").is_some());
        assert!(registry.get_in("cbor", "validate").is_none());
        assert!(registry.get_in("json", "decode").is_none());

        assert_eq!(registry.namespace_iter("cbor").count(), 2);
    }

    #[test]
    fn test_variant_selection() {
        fn generic(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::UInt(0)))
        }
        fn uint_variant(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::UInt(1)))
        }
        fn float_variant(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
            Ok(ExecResult::Return(Value::UInt(2)))
        }

        let def = ExternDef::new("math", "sqrt", generic)
            .param("x", TypeSet::numeric())
            .returns(TypeSet::numeric())
            .variant(&[TypeSet::uint()], TypeSet::uint(), uint_variant)
            .variant(
                &[TypeSet::single(crate::types::BaseType::Float)],
                TypeSet::single(crate::types::BaseType::Float),
                float_variant,
            );

        assert_eq!(def.variants.len(), 2);

        // UInt arg → selects uint_variant
        let v = def.select_variant(&[TypeSet::uint()]);
        assert!(v.is_some());
        assert!(v.unwrap().returns.contains(crate::types::BaseType::UInt));

        // Float arg → selects float_variant
        let v = def.select_variant(&[TypeSet::single(crate::types::BaseType::Float)]);
        assert!(v.is_some());
        assert!(v.unwrap().returns.contains(crate::types::BaseType::Float));

        // Numeric (union) → no variant matches (too broad)
        let v = def.select_variant(&[TypeSet::numeric()]);
        assert!(v.is_none());

        // All types → no variant matches
        let v = def.select_variant(&[TypeSet::any()]);
        assert!(v.is_none());

        // Wrong arity → no match
        let v = def.select_variant(&[TypeSet::uint(), TypeSet::uint()]);
        assert!(v.is_none());
    }
}
