// Core type definitions shared by IR and execution
//
// This module defines the fundamental types of the Zircon language.
// Both compile-time (IR) and runtime (exec) modules use these definitions.

/// The base types that a value can have at runtime.
///
/// These correspond to CBOR major types plus our language-specific distinctions:
/// - Bool, UInt, Int, Float: scalar types
/// - Text, Bytes: string-like types
/// - Array, Map: collection types
///
/// Note: "missing" is not a type - it's tracked orthogonally (Option<Value> at runtime,
/// TypeSet::maybe_undefined at compile time).
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
