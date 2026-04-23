//! Source and Library Loaders
//!
//! Traits for loading source files and pre-compiled bytecode during compilation.
//! The Compiler uses `SourceLoader` to resolve `import` statements; the Linker
//! uses `LibraryLoader` to load pre-compiled modules.
//!
//! # Provided Implementations
//!
//! - [`FileLoader`] — filesystem-based, resolves relative paths, canonical = absolute path
//! - [`MemoryLoader`] — in-memory map, canonical = key string

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ============================================================================
// Source Loader
// ============================================================================

/// Result of loading a source file.
pub struct SourceResult {
    /// UTF-8 source text
    pub source: String,
    /// Default namespace for this module (e.g., filename stem).
    /// Used when the import has no explicit `as` alias.
    pub namespace: String,
    /// Unique identity for deduplication. Two different relative paths resolving
    /// to the same file must produce the same canonical_id.
    pub canonical_id: String,
}

/// Load source text from an import identifier.
///
/// Used by the Compiler to resolve `import "path"` statements.
/// The loader resolves relative paths and returns a canonical identity
/// for deduplication — two different relative paths to the same file
/// must return the same `canonical_id`.
pub trait SourceLoader {
    /// Load source text.
    ///
    /// `identifier` is the import path (e.g., `"utils.rill"`, `"../common.rill"`).
    /// `from` is the canonical_id of the importing file (`None` for the root file).
    ///
    /// Returns the source text, default namespace, and canonical identity.
    fn load(&self, identifier: &str, from: Option<&str>) -> Result<SourceResult, String>;
}

// ============================================================================
// Library Loader
// ============================================================================

/// Load pre-compiled bytecode.
///
/// Used by the Linker to load `.rillc` files or equivalent serialized IR.
pub trait LibraryLoader {
    /// Load pre-compiled bytecode from an identifier.
    fn load(&self, identifier: &str) -> Result<Vec<u8>, String>;
}

// ============================================================================
// FileLoader
// ============================================================================

/// Filesystem-based source loader.
///
/// Resolves relative paths against the importing file's directory.
/// The canonical_id is the absolute path (canonicalized).
pub struct FileLoader {
    /// Base directory for resolving the root file's imports.
    /// If not set, uses the current working directory.
    base_dir: PathBuf,
}

impl FileLoader {
    /// Create a FileLoader with the given base directory.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        FileLoader {
            base_dir: base_dir.into(),
        }
    }

    /// Create a FileLoader using the current working directory.
    pub fn current_dir() -> Result<Self, String> {
        std::env::current_dir()
            .map(|dir| FileLoader { base_dir: dir })
            .map_err(|e| format!("failed to get current directory: {}", e))
    }

    /// Resolve an import path relative to the importing file.
    fn resolve(&self, identifier: &str, from: Option<&str>) -> Result<PathBuf, String> {
        let path = Path::new(identifier);

        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match from {
                Some(from_path) => {
                    // Resolve relative to the importing file's directory
                    let from_dir = Path::new(from_path).parent().unwrap_or(Path::new("."));
                    from_dir.join(path)
                }
                None => {
                    // Root file: resolve relative to base_dir
                    self.base_dir.join(path)
                }
            }
        };

        // Canonicalize to get the absolute path (resolves .., symlinks, etc.)
        resolved
            .canonicalize()
            .map_err(|e| format!("failed to resolve '{}': {}", identifier, e))
    }
}

impl SourceLoader for FileLoader {
    fn load(&self, identifier: &str, from: Option<&str>) -> Result<SourceResult, String> {
        let canonical = self.resolve(identifier, from)?;
        let canonical_str = canonical.to_string_lossy().to_string();

        let source = std::fs::read_to_string(&canonical)
            .map_err(|e| format!("failed to read '{}': {}", canonical_str, e))?;

        // Derive namespace from filename stem
        let namespace = canonical
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(SourceResult {
            source,
            namespace,
            canonical_id: canonical_str,
        })
    }
}

// ============================================================================
// MemoryLoader
// ============================================================================

/// In-memory source loader for testing and embedding.
///
/// Sources are pre-loaded into a map. The canonical_id is the key string.
/// Relative path resolution is not performed — identifiers must match
/// keys exactly.
pub struct MemoryLoader {
    sources: HashMap<String, (String, String)>, // key → (source, namespace)
}

impl MemoryLoader {
    /// Create an empty MemoryLoader.
    pub fn new() -> Self {
        MemoryLoader {
            sources: HashMap::new(),
        }
    }

    /// Add a source file to the loader.
    ///
    /// `identifier` is both the import path and the canonical_id.
    /// `namespace` is the default namespace (typically the filename stem).
    pub fn add(
        &mut self,
        identifier: impl Into<String>,
        namespace: impl Into<String>,
        source: impl Into<String>,
    ) -> &mut Self {
        let id = identifier.into();
        self.sources.insert(id, (source.into(), namespace.into()));
        self
    }

    /// Add a source file, deriving the namespace from the identifier.
    ///
    /// The namespace is the filename stem of the identifier (e.g.,
    /// "utils.rill" → "utils", "path/to/helpers.rill" → "helpers").
    pub fn add_source(
        &mut self,
        identifier: impl Into<String>,
        source: impl Into<String>,
    ) -> &mut Self {
        let id = identifier.into();
        let namespace = Path::new(&id)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&id)
            .to_string();
        self.sources.insert(id, (source.into(), namespace));
        self
    }
}

impl Default for MemoryLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceLoader for MemoryLoader {
    fn load(&self, identifier: &str, _from: Option<&str>) -> Result<SourceResult, String> {
        match self.sources.get(identifier) {
            Some((source, namespace)) => Ok(SourceResult {
                source: source.clone(),
                namespace: namespace.clone(),
                canonical_id: identifier.to_string(),
            }),
            None => Err(format!("source not found: '{}'", identifier)),
        }
    }
}
