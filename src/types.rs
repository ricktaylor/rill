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
///
/// Note: "missing" is not a type — it's tracked orthogonally (Option<Value> at runtime,
/// Definedness lattice at compile time).
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
}

impl BaseType {
    /// Returns the type name as used in source code (for error messages)
    pub fn name(&self) -> &'static str {
        match self {
            BaseType::Bool => "Bool",
            BaseType::UInt => "UInt",
            BaseType::Int => "Int",
            BaseType::Float => "Float",
            BaseType::Text => "Text",
            BaseType::Bytes => "Bytes",
            BaseType::Array => "Array",
            BaseType::Map => "Map",
            BaseType::Sequence => "Sequence",
        }
    }

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
    const ALL: [BaseType; 9] = [
        BaseType::Bool,
        BaseType::UInt,
        BaseType::Int,
        BaseType::Float,
        BaseType::Text,
        BaseType::Bytes,
        BaseType::Array,
        BaseType::Map,
        BaseType::Sequence,
    ];
}

// ============================================================================
// TypeSet - Set of possible types
// ============================================================================

/// A set of possible types for a value, stored as a compact bitfield.
///
/// Used throughout the compiler for:
/// - Builtin parameter and return type signatures
/// - IR type analysis and refinement
/// - Type checking and inference
///
/// Internally uses a `u16` with one bit per `BaseType` (9 types = 9 bits).
/// All operations are O(1) with no heap allocation.
///
/// Note: Definedness (whether a value might be undefined/missing) is tracked
/// orthogonally via the Definedness lattice, not in TypeSet.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TypeSet {
    bits: u16,
}

impl std::fmt::Debug for TypeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let types: Vec<&str> = BaseType::ALL
            .iter()
            .filter(|t| self.contains(**t))
            .map(|t| t.name())
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

    /// Create a type set containing all types
    pub const fn all() -> Self {
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
