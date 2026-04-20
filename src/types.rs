//! Core type definitions shared by IR and execution
//!
//! This module defines the fundamental types of the Rill language.
//! Both compile-time (IR) and runtime (exec) modules use these definitions.

/// The base types that a value can have at runtime.
///
/// Duck-typed value system covering common data interchange types:
/// - Bool, UInt, Int, Float: scalar types
/// - Text, Bytes: string-like types
/// - Array, Map: collection types
/// - Sequence: internal lazy iterator (not user-visible)
/// - Undefined: absence of a value (failed operations, missing data)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BaseType {
    Bool,
    UInt,
    Int,
    Float,
    Text,
    Bytes,
    Array,
    Map,
    Sequence,
    Undefined,
}

impl core::fmt::Display for BaseType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            BaseType::Bool => "Bool",
            BaseType::UInt => "UInt",
            BaseType::Int => "Int",
            BaseType::Float => "Float",
            BaseType::Text => "Text",
            BaseType::Bytes => "Bytes",
            BaseType::Array => "Array",
            BaseType::Map => "Map",
            BaseType::Sequence => "Sequence",
            BaseType::Undefined => "Undefined",
        })
    }
}

impl BaseType {
    /// Check if this is a numeric type
    pub fn is_numeric(&self) -> bool {
        matches!(self, BaseType::UInt | BaseType::Int | BaseType::Float)
    }

    /// Check if this is a collection type (indexable)
    pub fn is_collection(&self) -> bool {
        matches!(
            self,
            BaseType::Array | BaseType::Map | BaseType::Text | BaseType::Bytes
        )
    }

    /// Check if this type can be iterated with `for`
    pub fn is_iterable(&self) -> bool {
        matches!(
            self,
            BaseType::Array | BaseType::Map | BaseType::Text | BaseType::Bytes | BaseType::Sequence
        )
    }

    /// Check if this is an integer type
    pub fn is_integer(&self) -> bool {
        matches!(self, BaseType::UInt | BaseType::Int)
    }

    /// Bit position for this type in a TypeSet bitfield.
    /// Supports up to 16 types (u16). Adding more requires widening the bitfield.
    const fn bit(self) -> u16 {
        // Compile-time guard: if this panics, the bitfield type needs widening
        assert!(
            (self as u16) < 16,
            "BaseType has too many variants for u16 bitfield"
        );
        1 << (self as u16)
    }

    /// All base type variants, for iteration
    const ALL: [BaseType; 10] = [
        BaseType::Bool,
        BaseType::UInt,
        BaseType::Int,
        BaseType::Float,
        BaseType::Text,
        BaseType::Bytes,
        BaseType::Array,
        BaseType::Map,
        BaseType::Sequence,
        BaseType::Undefined,
    ];
}

// ============================================================================
// NumericType - Target type for numeric conversions
// ============================================================================

/// Target type for numeric conversion operations.
///
/// This is the numeric subset of `BaseType`, used as a compile-time parameter
/// on the `IntrinsicOp::Convert` variant. The target type is always statically
/// known at IR construction time — either from the `as Type` syntax or from
/// the coercion pass's promotion lattice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumericType {
    UInt,
    Int,
    Float,
}

impl core::fmt::Display for NumericType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            NumericType::UInt => "UInt",
            NumericType::Int => "Int",
            NumericType::Float => "Float",
        })
    }
}

/// Mode for numeric conversion operations.
///
/// - `Checked`: compiler-inserted promotion along the widening lattice
///   (UInt < Int < Float). UInt→Int is overflow-checked — values > i64::MAX
///   produce undefined. Only goes "up" the lattice.
/// - `Unchecked`: user-requested via `value as Type`. Bit-reinterprets for
///   Int↔UInt, always succeeds for valid numeric pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConvertMode {
    Checked,
    Unchecked,
}

/// Mutability mode for array slice sequences.
///
/// Controls whether a `..rest` destructuring pattern yields elements
/// by value or by reference (with write-back to the source array).
/// Known at compile time from the binding mode (`let` vs `with`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SliceMode {
    /// `let [a, ..rest] = arr` — elements yielded by value, no write-back
    ReadOnly,
    /// `with [a, ..rest] = arr` — for-loop uses MakeRef, mutations write back
    Mutable,
}

impl From<NumericType> for BaseType {
    fn from(nt: NumericType) -> BaseType {
        match nt {
            NumericType::UInt => BaseType::UInt,
            NumericType::Int => BaseType::Int,
            NumericType::Float => BaseType::Float,
        }
    }
}

impl From<BaseType> for NumericType {
    fn from(bt: BaseType) -> NumericType {
        match bt {
            BaseType::UInt => NumericType::UInt,
            BaseType::Int => NumericType::Int,
            BaseType::Float => NumericType::Float,
            _ => unreachable!("NumericType::from called with non-numeric BaseType::{bt}"),
        }
    }
}

// ============================================================================
// TypeSet - Set of possible types
// ============================================================================

/// A set of possible types for a value, stored as a compact bitfield.
///
/// Used throughout the compiler for:
/// - Extern parameter and return type signatures
/// - IR type analysis and refinement
/// - Type checking and inference
///
/// Internally uses a `u16` with one bit per `BaseType` (10 types = 10 bits).
/// All operations are O(1) with no heap allocation.
///
/// Undefined is a type in the set. `TypeSet::defined()` is the set of all
/// value types (excludes Undefined). `TypeSet::any()` is the true top
/// (includes Undefined). Specific constructors like `numeric()`, `bool()`
/// etc. exclude Undefined — they imply definedness.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TypeSet {
    bits: u16,
}

impl core::fmt::Debug for TypeSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let types: Vec<_> = BaseType::ALL
            .iter()
            .filter(|t| self.contains(**t))
            .map(|t| t.to_string())
            .collect();
        write!(f, "TypeSet{{{}}}", types.join(", "))
    }
}

impl TypeSet {
    /// Create an empty type set (represents unreachable/bottom)
    pub const fn empty() -> Self {
        TypeSet { bits: 0 }
    }

    /// Create a type set containing a single type
    pub const fn single(ty: BaseType) -> Self {
        TypeSet { bits: ty.bit() }
    }

    /// Create a type set from multiple types
    pub fn from_types(types: impl IntoIterator<Item = BaseType>) -> Self {
        let mut bits = 0u16;
        for ty in types {
            bits |= ty.bit();
        }
        TypeSet { bits }
    }

    /// All value types, excluding Undefined. Use when a value must be
    /// defined but could be any type (e.g. Eq operands, collection elements).
    pub const fn defined() -> Self {
        TypeSet {
            bits: BaseType::Bool.bit()
                | BaseType::UInt.bit()
                | BaseType::Int.bit()
                | BaseType::Float.bit()
                | BaseType::Text.bit()
                | BaseType::Bytes.bit()
                | BaseType::Array.bit()
                | BaseType::Map.bit()
                | BaseType::Sequence.bit(),
        }
    }

    /// True top — any type including Undefined. Use for uninitialized
    /// variables or values whose definedness is unknown.
    pub const fn any() -> Self {
        TypeSet {
            bits: Self::defined().bits | BaseType::Undefined.bit(),
        }
    }

    /// Provably undefined — exactly `{Undefined}`.
    pub const fn undefined() -> Self {
        Self::single(BaseType::Undefined)
    }

    /// Temporary alias during migration — will be removed once all call
    /// sites are reviewed and switched to `defined()` or `any()`.
    /// Uses `any()` (includes Undefined) so that unknown variables are
    /// correctly modeled as possibly-undefined by type analysis.
    pub const fn all() -> Self {
        Self::any()
    }

    // Convenience constructors

    pub const fn bool() -> Self {
        Self::single(BaseType::Bool)
    }
    pub const fn uint() -> Self {
        Self::single(BaseType::UInt)
    }
    pub const fn int() -> Self {
        Self::single(BaseType::Int)
    }
    pub const fn float() -> Self {
        Self::single(BaseType::Float)
    }
    pub const fn text() -> Self {
        Self::single(BaseType::Text)
    }
    pub const fn bytes() -> Self {
        Self::single(BaseType::Bytes)
    }
    pub const fn array() -> Self {
        Self::single(BaseType::Array)
    }
    pub const fn map() -> Self {
        Self::single(BaseType::Map)
    }
    pub const fn sequence() -> Self {
        Self::single(BaseType::Sequence)
    }

    pub const fn numeric() -> Self {
        TypeSet {
            bits: BaseType::UInt.bit() | BaseType::Int.bit() | BaseType::Float.bit(),
        }
    }

    pub const fn integer() -> Self {
        TypeSet {
            bits: BaseType::UInt.bit() | BaseType::Int.bit(),
        }
    }

    pub const fn collection() -> Self {
        TypeSet {
            bits: BaseType::Array.bit()
                | BaseType::Map.bit()
                | BaseType::Text.bit()
                | BaseType::Bytes.bit(),
        }
    }

    pub const fn iterable() -> Self {
        TypeSet {
            bits: BaseType::Array.bit()
                | BaseType::Map.bit()
                | BaseType::Text.bit()
                | BaseType::Bytes.bit()
                | BaseType::Sequence.bit(),
        }
    }

    // Set operations

    /// Union of two type sets (for phi nodes, joins)
    pub const fn union(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            bits: self.bits | other.bits,
        }
    }

    /// Intersection of two type sets (for refinement)
    pub const fn intersection(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            bits: self.bits & other.bits,
        }
    }

    /// Difference: types in self but not in other
    pub const fn difference(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            bits: self.bits & !other.bits,
        }
    }

    // Queries

    /// Check if type set contains a specific type
    pub const fn contains(&self, ty: BaseType) -> bool {
        self.bits & ty.bit() != 0
    }

    /// Check if type set is empty (unreachable/bottom)
    pub const fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Check if type set contains exactly one type
    pub const fn is_single(&self) -> bool {
        self.bits != 0 && (self.bits & (self.bits - 1)) == 0
    }

    /// Get the single type if this set contains exactly one
    pub fn as_single(&self) -> Option<BaseType> {
        if !self.is_single() {
            return None;
        }
        BaseType::ALL.iter().find(|t| self.contains(**t)).copied()
    }

    /// Check if this is a boolean type (exactly Bool)
    pub const fn is_bool(&self) -> bool {
        self.bits == BaseType::Bool.bit()
    }

    /// Check if this type set excludes Undefined (value is provably defined)
    pub const fn is_defined(&self) -> bool {
        self.bits & BaseType::Undefined.bit() == 0 && self.bits != 0
    }

    /// Check if this might be undefined
    pub const fn may_be_undefined(&self) -> bool {
        self.bits & BaseType::Undefined.bit() != 0
    }

    /// Check if all types are numeric
    pub const fn is_numeric(&self) -> bool {
        self.bits != 0 && self.bits & !Self::numeric().bits == 0
    }

    /// Check if all types are integers
    pub const fn is_integer(&self) -> bool {
        self.bits != 0 && self.bits & !Self::integer().bits == 0
    }

    /// Check if all types are collections
    pub const fn is_collection(&self) -> bool {
        self.bits != 0 && self.bits & !Self::collection().bits == 0
    }

    /// Iterate over the types in this set
    pub fn iter(&self) -> impl Iterator<Item = BaseType> + '_ {
        BaseType::ALL.iter().filter(|t| self.contains(**t)).copied()
    }

    /// Number of types in this set
    pub const fn len(&self) -> usize {
        self.bits.count_ones() as usize
    }
}
