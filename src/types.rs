//! Core type definitions shared by IR and execution
//!
//! This module defines the fundamental types of the Rill language.
//! Both compile-time (IR) and runtime (exec) modules use these definitions.

use std::collections::BTreeSet;

/// The base types that a value can have at runtime.
///
/// These correspond to CBOR major types plus our language-specific distinctions:
/// - Bool, UInt, Int, Float: scalar types
/// - Text, Bytes: string-like types
/// - Array, Map: collection types
///
/// Note: "missing" is not a type - it's tracked orthogonally (Option<Value> at runtime,
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
        }
    }

    /// Check if this is a numeric type
    pub fn is_numeric(&self) -> bool {
        matches!(self, BaseType::UInt | BaseType::Int | BaseType::Float)
    }

    /// Check if this is a collection type
    pub fn is_collection(&self) -> bool {
        matches!(
            self,
            BaseType::Array | BaseType::Map | BaseType::Text | BaseType::Bytes
        )
    }

    /// Check if this is an integer type
    pub fn is_integer(&self) -> bool {
        matches!(self, BaseType::UInt | BaseType::Int)
    }
}

// ============================================================================
// TypeSet - Set of possible types
// ============================================================================

/// A set of possible types for a value.
///
/// Used throughout the compiler for:
/// - Builtin parameter and return type signatures
/// - IR type analysis and refinement
/// - Type checking and inference
///
/// Note: Definedness (whether a value might be undefined/missing) is tracked
/// orthogonally via the Definedness lattice, not in TypeSet.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeSet {
    types: BTreeSet<BaseType>,
}

impl TypeSet {
    /// Create an empty type set (represents unreachable/bottom)
    pub fn empty() -> Self {
        TypeSet {
            types: BTreeSet::new(),
        }
    }

    /// Create a type set containing a single type
    pub fn single(ty: BaseType) -> Self {
        let mut types = BTreeSet::new();
        types.insert(ty);
        TypeSet { types }
    }

    /// Create a type set from multiple types
    pub fn from_types(types: impl IntoIterator<Item = BaseType>) -> Self {
        TypeSet {
            types: types.into_iter().collect(),
        }
    }

    /// Create a type set containing all types
    pub fn all() -> Self {
        TypeSet::from_types([
            BaseType::Bool,
            BaseType::UInt,
            BaseType::Int,
            BaseType::Float,
            BaseType::Text,
            BaseType::Bytes,
            BaseType::Array,
            BaseType::Map,
        ])
    }

    // Convenience constructors matching common signatures

    /// Bool type
    pub fn bool() -> Self {
        Self::single(BaseType::Bool)
    }

    /// UInt type
    pub fn uint() -> Self {
        Self::single(BaseType::UInt)
    }

    /// Int type
    pub fn int() -> Self {
        Self::single(BaseType::Int)
    }

    /// Float type
    pub fn float() -> Self {
        Self::single(BaseType::Float)
    }

    /// Text type
    pub fn text() -> Self {
        Self::single(BaseType::Text)
    }

    /// Bytes type
    pub fn bytes() -> Self {
        Self::single(BaseType::Bytes)
    }

    /// Array type
    pub fn array() -> Self {
        Self::single(BaseType::Array)
    }

    /// Map type
    pub fn map() -> Self {
        Self::single(BaseType::Map)
    }

    /// Any numeric type (UInt, Int, Float)
    pub fn numeric() -> Self {
        TypeSet::from_types([BaseType::UInt, BaseType::Int, BaseType::Float])
    }

    /// Any integer type (UInt, Int)
    pub fn integer() -> Self {
        TypeSet::from_types([BaseType::UInt, BaseType::Int])
    }

    /// Any collection type (Array, Map, Text, Bytes)
    pub fn collection() -> Self {
        TypeSet::from_types([
            BaseType::Array,
            BaseType::Map,
            BaseType::Text,
            BaseType::Bytes,
        ])
    }

    // Set operations

    /// Union of two type sets (for phi nodes, joins)
    pub fn union(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            types: self.types.union(&other.types).copied().collect(),
        }
    }

    /// Intersection of two type sets (for refinement)
    pub fn intersection(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            types: self.types.intersection(&other.types).copied().collect(),
        }
    }

    /// Difference: types in self but not in other
    pub fn difference(&self, other: &TypeSet) -> TypeSet {
        TypeSet {
            types: self.types.difference(&other.types).copied().collect(),
        }
    }

    // Queries

    /// Check if type set contains a specific type
    pub fn contains(&self, ty: BaseType) -> bool {
        self.types.contains(&ty)
    }

    /// Check if type set is empty (unreachable/bottom)
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Check if type set contains exactly one type
    pub fn is_single(&self) -> bool {
        self.types.len() == 1
    }

    /// Get the single type if this set contains exactly one
    pub fn as_single(&self) -> Option<BaseType> {
        if self.types.len() == 1 {
            self.types.iter().next().copied()
        } else {
            None
        }
    }

    /// Check if this is a boolean type (exactly Bool)
    pub fn is_bool(&self) -> bool {
        self.types.len() == 1 && self.types.contains(&BaseType::Bool)
    }

    /// Check if all types are numeric
    pub fn is_numeric(&self) -> bool {
        !self.types.is_empty() && self.types.iter().all(|t| t.is_numeric())
    }

    /// Check if all types are integers
    pub fn is_integer(&self) -> bool {
        !self.types.is_empty() && self.types.iter().all(|t| t.is_integer())
    }

    /// Check if all types are collections
    pub fn is_collection(&self) -> bool {
        !self.types.is_empty() && self.types.iter().all(|t| t.is_collection())
    }

    /// Iterate over the types in this set
    pub fn iter(&self) -> impl Iterator<Item = BaseType> + '_ {
        self.types.iter().copied()
    }

    /// Number of types in this set
    pub fn len(&self) -> usize {
        self.types.len()
    }
}
