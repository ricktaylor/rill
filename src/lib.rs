mod ast;
mod compile;
pub mod diagnostics;
pub mod exec;
pub mod externs;
mod ir;
pub mod loader;
mod opt;
mod ssa;
pub mod types;

// Re-export key types for convenient access
pub use diagnostics::{Diagnostics, LineCol, SourceMap, offset_to_line_col, span_to_line_col};
pub use exec::{ExecError, VM, Value};
pub use externs::{ExternDef, ExternRegistry, RegistryError};
pub use loader::{FileLoader, MemoryLoader, SourceLoader, SourceResult};
pub use types::{BaseType, TypeSet};

/// Compiled Rill program, ready for execution.
///
/// This is an opaque handle produced by [`compile()`]. The internal
/// representation may change between versions — do not depend on its
/// structure. A future serialization format will allow saving/loading
/// compiled programs without re-compilation.
pub struct Program {
    compiled: compile::CompiledProgram,
}

impl Program {
    /// Resolve a function by name, returning a handle for repeated calls.
    ///
    /// Performs the name lookup once. The returned [`Function`] can be called
    /// many times without further lookup overhead — critical for hot-path
    /// embedding where the same program processes many inputs.
    ///
    /// ```ignore
    /// let process = program.function("process").expect("function exists");
    /// for input in inputs {
    ///     vm.push(input)?;
    ///     let result = process.call(&mut vm, 1)?;
    /// }
    /// ```
    pub fn function(&self, name: &str) -> Option<FunctionHandle<'_>> {
        self.compiled
            .func_index
            .get(name)
            .map(|&idx| FunctionHandle {
                program: &self.compiled,
                func_idx: idx,
            })
    }

    /// Call a named function (convenience method — does a name lookup each time).
    ///
    /// Push arguments onto the VM stack before calling:
    /// ```ignore
    /// vm.push(Value::UInt(42))?;
    /// vm.push(Value::Text("hello".into()))?;
    /// let result = program.call(&mut vm, "process", 2)?;
    /// ```
    ///
    /// For repeated calls to the same function, use [`function()`] to resolve
    /// the name once and then call the returned handle.
    pub fn call(&self, vm: &mut VM, func_name: &str, argc: usize) -> Result<Value, ExecError> {
        compile::execute(&self.compiled, vm, func_name, argc)
    }
}

impl VM {
    /// Reset this VM and initialize a program's file-scope globals.
    ///
    /// Reserves the program's global slots (0..N) and runs its synthetic
    /// `__init__` to evaluate the initializers in source order, leaving the VM
    /// ready for [`Program::call`]. Call this once after [`compile()`], before
    /// invoking any function in a program that declares globals. For global-free
    /// programs it is a cheap reset.
    pub fn exec(&mut self, program: &Program) -> Result<(), ExecError> {
        self.reserve_globals(program.compiled.global_count);
        if let Some(init_idx) = program.compiled.init_func {
            compile::execute_by_index(&program.compiled, self, init_idx, 0)?;
        }
        Ok(())
    }
}

/// A resolved function handle — no name lookup on each call.
///
/// Obtained from [`Program::function()`]. Holds a reference to the program
/// and the resolved function index. Use this for hot-path execution where
/// the same function is called repeatedly with different data.
pub struct FunctionHandle<'a> {
    program: &'a compile::CompiledProgram,
    func_idx: usize,
}

impl<'a> FunctionHandle<'a> {
    /// Execute this function with the given arguments.
    ///
    /// Push arguments onto the VM stack before calling:
    /// ```ignore
    /// vm.push(value)?;
    /// let result = handle.call(&mut vm, 1)?;
    /// ```
    pub fn call(&self, vm: &mut VM, argc: usize) -> Result<Value, ExecError> {
        compile::execute_by_index(self.program, vm, self.func_idx, argc)
    }
}

/// Compile source code into an executable program.
///
/// Runs the full pipeline: parse → lower → optimize → compile to closures.
///
/// Returns `Ok((program, diagnostics))` on success (diagnostics may contain
/// warnings), or `Err(diagnostics)` if there were compilation errors.
pub fn compile(
    source: &str,
    externs: &ExternRegistry,
) -> Result<(Program, Diagnostics), Diagnostics> {
    let mut diagnostics = Diagnostics::new();
    let source_id: std::rc::Rc<str> = std::rc::Rc::from("<input>");
    diagnostics
        .source_map
        .add(source_id.clone(), source.to_string());
    diagnostics.set_source(source_id);

    let ast = match ast::parser::parse(source, "<input>", &mut diagnostics) {
        Some(ast) => ast,
        None => return Err(diagnostics),
    };

    let mut ir_program = match ir::lower(&ast, externs, &mut diagnostics) {
        Some(ir) => ir,
        None => return Err(diagnostics),
    };

    opt::optimize(&mut ir_program, externs, &mut diagnostics);

    // Compile IR to closure-threaded code (includes link phase)
    let mut compiled = match compile::compile_program(&ir_program, externs) {
        Ok(compiled) => compiled,
        Err(link_errors) => {
            diagnostics.merge(link_errors);
            return Err(diagnostics);
        }
    };

    // Merge any link-phase warnings (unused functions, etc.)
    diagnostics.merge(std::mem::take(&mut compiled.warnings));

    Ok((Program { compiled }, diagnostics))
}

/// Create an extern registry with standard externs registered.
pub fn standard_externs() -> ExternRegistry {
    externs::standard_externs()
}

// ============================================================================
// Compiler Builder
// ============================================================================

/// Builder for compiling Rill source into executable programs.
///
/// Takes a single SourceLoader at construction time — all source files
/// are loaded through it, ensuring canonical_id consistency.
///
/// # Example
///
/// ```ignore
/// let loader = FileLoader::new("./scripts");
/// let mut compiler = Compiler::new(&loader);
/// compiler.add_extern(ExternDef::new("math", "sqrt", sqrt_impl))?;
/// compiler.add("main.rill");
/// let (program, warnings) = compiler.build()?;
/// ```
///
/// A parsed (but not yet lowered) source file with its import metadata.
struct ParsedSource {
    ast: ast::AstProgram,
    canonical_id: String,
    /// Import namespace mappings: (canonical_id of imported file) → (namespace alias)
    /// None alias = `as _` (root merge).
    import_aliases: Vec<(String, Option<String>)>,
}

/// Metadata for a loaded and lowered source file.
struct LoadedSource {
    /// The lowered IR program
    ir: ir::IrProgram,
    /// The canonical_id of this source
    canonical_id: String,
    /// Import namespace mappings: (canonical_id of imported file) → (namespace alias)
    /// None alias = `as _` (root merge).
    import_aliases: Vec<(String, Option<String>)>,
}

pub struct Compiler<'a> {
    externs: ExternRegistry,
    /// The source loader — one per Compiler for canonical_id consistency
    loader: &'a dyn SourceLoader,
    /// Accumulated loaded sources with their metadata
    sources: Vec<LoadedSource>,
    /// Diagnostics accumulated during add() calls
    diagnostics: Diagnostics,
    /// Canonical IDs already loaded (for deduplication)
    loaded: std::collections::HashSet<String>,
    /// Map from canonical_id to default namespace (from SourceLoader)
    default_namespaces: std::collections::HashMap<String, String>,
}

impl<'a> Compiler<'a> {
    /// Create a new Compiler with the given source loader.
    ///
    /// A single loader ensures canonical_id consistency across all source files.
    pub fn new(loader: &'a dyn SourceLoader) -> Self {
        Compiler {
            externs: ExternRegistry::new(),
            loader,
            sources: Vec::new(),
            diagnostics: Diagnostics::new(),
            loaded: std::collections::HashSet::new(),
            default_namespaces: std::collections::HashMap::new(),
        }
    }

    /// Create a new Compiler with the given extern registry and source loader.
    pub fn with_externs(externs: ExternRegistry, loader: &'a dyn SourceLoader) -> Self {
        Compiler {
            externs,
            loader,
            sources: Vec::new(),
            diagnostics: Diagnostics::new(),
            loaded: std::collections::HashSet::new(),
            default_namespaces: std::collections::HashMap::new(),
        }
    }

    /// Register an extern function.
    ///
    /// The namespace and name come from the `ExternDef`.
    /// Scripts use `require namespace;` to access as `namespace::func()`,
    /// or `require namespace as _;` to merge into root scope.
    pub fn add_extern(&mut self, def: externs::ExternDef) -> Result<(), RegistryError> {
        self.externs.register(def)
    }

    /// Add a source file by identifier.
    ///
    /// Loads the source via the Compiler's SourceLoader, parses it, resolves
    /// any `import` statements recursively, and lowers to IR. Errors are
    /// accumulated — call `build()` to check for errors.
    ///
    /// `identifier` is the root file path (e.g., `"main.rill"`).
    pub fn add(&mut self, identifier: &str) -> &mut Self {
        // Load root source
        let result = match self.loader.load(identifier, None) {
            Ok(r) => r,
            Err(e) => {
                self.diagnostics.error_no_span(
                    diagnostics::DiagnosticCode::E500_UndefinedExternal,
                    format!("failed to load '{}': {}", identifier, e),
                );
                return self;
            }
        };

        // Process root + imports via BFS queue
        self.process_source(result);
        self
    }

    /// Add source text directly (without a loader).
    ///
    /// Convenience for single-file compilation or testing.
    /// No imports are resolved (import statements will produce errors).
    pub fn add_source(&mut self, source: &str, source_id: &str) -> &mut Self {
        let result = SourceResult {
            source: source.to_string(),
            namespace: String::new(),
            canonical_id: source_id.to_string(),
        };
        self.process_source_single(result);
        self
    }

    /// Process a source file and its imports.
    ///
    /// Two-pass approach:
    /// 1. BFS load + parse all files, collecting ASTs and import metadata
    /// 2. Lower each file with `merged_imports` populated from `as _` imports
    fn process_source(&mut self, root: SourceResult) {
        let parsed = self.parse_source_tree(root);
        self.lower_parsed_sources(parsed);
    }

    /// Pass 1: BFS load and parse all source files.
    fn parse_source_tree(&mut self, root: SourceResult) -> Vec<ParsedSource> {
        use std::collections::VecDeque;
        use std::rc::Rc;

        let mut parsed: Vec<ParsedSource> = Vec::new();
        let mut queue: VecDeque<SourceResult> = VecDeque::new();

        if !self.loaded.insert(root.canonical_id.clone()) {
            return parsed;
        }
        self.default_namespaces
            .insert(root.canonical_id.clone(), root.namespace.clone());
        self.diagnostics
            .source_map
            .add(Rc::from(root.canonical_id.as_str()), root.source.clone());
        queue.push_back(root);

        while let Some(source_result) = queue.pop_front() {
            let canonical_id = source_result.canonical_id.clone();

            // Set current source so parse errors are tagged
            self.diagnostics.set_source(Rc::from(canonical_id.as_str()));

            let ast = match ast::parser::parse(
                &source_result.source,
                &canonical_id,
                &mut self.diagnostics,
            ) {
                Some(ast) => ast,
                None => continue,
            };

            // Collect require namespace aliases from this file's AST
            // (used to detect import vs require clashes)
            let require_aliases: std::collections::HashSet<String> = ast
                .requires
                .iter()
                .filter_map(|r| match &r.node.alias {
                    Some(a) if a.0 == "_" => None,
                    Some(a) => Some(a.0.clone()),
                    None => Some(r.node.namespace.0.clone()),
                })
                .collect();

            let mut import_aliases = Vec::new();
            let mut seen_aliases: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for import in &ast.imports {
                let import_path = &import.node.path;
                match self.loader.load(import_path, Some(&canonical_id)) {
                    Ok(imported) => {
                        let imported_canonical = imported.canonical_id.clone();
                        let imported_ns = imported.namespace.clone();

                        let namespace = match &import.node.alias {
                            Some(alias) if alias.0 == "_" => None, // root merge
                            Some(alias) => Some(alias.0.clone()),
                            None => Some(imported_ns.clone()),
                        };

                        if let Some(ref ns) = namespace {
                            if !seen_aliases.insert(ns.clone()) {
                                self.diagnostics.error(
                                    diagnostics::DiagnosticCode::E400_DuplicateDefinition,
                                    import.span,
                                    format!(
                                        "duplicate import namespace `{}`; \
                                         use `as` to disambiguate",
                                        ns,
                                    ),
                                );
                                continue;
                            }
                            if require_aliases.contains(ns) {
                                self.diagnostics.error(
                                    diagnostics::DiagnosticCode::E400_DuplicateDefinition,
                                    import.span,
                                    format!(
                                        "import namespace `{}` clashes with \
                                         require namespace; use `as` to disambiguate",
                                        ns,
                                    ),
                                );
                                continue;
                            }
                        }

                        import_aliases.push((imported_canonical.clone(), namespace));

                        if self.loaded.insert(imported_canonical.clone()) {
                            self.default_namespaces
                                .insert(imported_canonical, imported_ns);
                            self.diagnostics.source_map.add(
                                Rc::from(imported.canonical_id.as_str()),
                                imported.source.clone(),
                            );
                            queue.push_back(imported);
                        }
                    }
                    Err(e) => {
                        self.diagnostics.error(
                            diagnostics::DiagnosticCode::E500_UndefinedExternal,
                            import.span,
                            format!("failed to import '{}': {}", import_path, e),
                        );
                    }
                }
            }

            parsed.push(ParsedSource {
                ast,
                canonical_id,
                import_aliases,
            });
        }

        parsed
    }

    /// Pass 2: lower each parsed file, with `merged_imports` for `as _` imports.
    fn lower_parsed_sources(&mut self, parsed: Vec<ParsedSource>) {
        use std::collections::HashMap;

        // Build canonical_id → function names index from parsed ASTs (owned)
        let file_functions: HashMap<String, Vec<String>> = parsed
            .iter()
            .map(|p| {
                let names = p
                    .ast
                    .functions
                    .iter()
                    .map(|f| f.node.name.0.clone())
                    .collect();
                (p.canonical_id.clone(), names)
            })
            .collect();

        for p in parsed {
            // Build merged_imports for this file's `as _` imports.
            // Resolution order for unqualified names:
            //   1. Intrinsics (len, collect, append)
            //   2. Local user functions (same file)
            //   3. Merged imports (import "x" as _)  ← this map
            //   4. Externs — global and merged (require ns as _)
            let mut merged_imports: HashMap<ast::Identifier, ast::Identifier> = HashMap::new();
            for (imported_canonical_id, alias) in &p.import_aliases {
                if alias.is_none() {
                    let canonical_ns = self
                        .default_namespaces
                        .get(imported_canonical_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());

                    if let Some(func_names) = file_functions.get(imported_canonical_id) {
                        for name in func_names {
                            merged_imports.insert(
                                ast::Identifier(name.clone()),
                                ast::Identifier(canonical_ns.clone()),
                            );
                        }
                    }
                }
            }

            if let Some(ir_program) =
                ir::lower_with_imports(&p.ast, &self.externs, &mut self.diagnostics, merged_imports)
            {
                self.sources.push(LoadedSource {
                    ir: ir_program,
                    canonical_id: p.canonical_id,
                    import_aliases: p.import_aliases,
                });
            }
        }
    }

    /// Process a single source file (no import resolution).
    fn process_source_single(&mut self, source_result: SourceResult) {
        use std::rc::Rc;

        let canonical_id = source_result.canonical_id.clone();
        if !self.loaded.insert(canonical_id.clone()) {
            return;
        }

        self.default_namespaces
            .insert(canonical_id.clone(), source_result.namespace.clone());
        self.diagnostics.source_map.add(
            Rc::from(canonical_id.as_str()),
            source_result.source.clone(),
        );

        let ast =
            match ast::parser::parse(&source_result.source, &canonical_id, &mut self.diagnostics) {
                Some(ast) => ast,
                None => return,
            };

        if let Some(ir_program) = ir::lower(&ast, &self.externs, &mut self.diagnostics) {
            self.sources.push(LoadedSource {
                ir: ir_program,
                canonical_id,
                import_aliases: Vec::new(),
            });
        }
    }

    /// Build the program — merges all IR, optimizes, and compiles to closures.
    ///
    /// Returns `Ok((Program, Diagnostics))` on success (diagnostics may contain
    /// warnings), or `Err(Diagnostics)` if there were compilation errors.
    pub fn build(&self) -> Result<(Program, Diagnostics), Diagnostics> {
        let mut diagnostics = Diagnostics::new();
        // Clone accumulated diagnostics (build doesn't consume the compiler)
        for diag in self.diagnostics.iter() {
            diagnostics.emit(diag.clone());
        }
        diagnostics.source_map = self.diagnostics.source_map.clone();

        if diagnostics.has_errors() {
            return Err(diagnostics);
        }

        // Merge all IR programs with namespace remapping
        let merged = self.merge_ir(&mut diagnostics);

        // Merge can surface cross-file errors (duplicate definitions, unsupported
        // imported globals) — stop before optimizing/compiling invalid IR.
        if diagnostics.has_errors() {
            return Err(diagnostics);
        }

        // Optimize
        let mut ir_program = merged;
        opt::optimize(&mut ir_program, &self.externs, &mut diagnostics);

        // Compile IR to closure-threaded code
        let mut compiled = match compile::compile_program(&ir_program, &self.externs) {
            Ok(compiled) => compiled,
            Err(link_errors) => {
                diagnostics.merge(link_errors);
                return Err(diagnostics);
            }
        };

        diagnostics.merge(std::mem::take(&mut compiled.warnings));

        Ok((Program { compiled }, diagnostics))
    }

    /// Merge all accumulated IR programs into a single program.
    ///
    /// For multi-file programs:
    /// 1. Functions from imported files are prefixed with their canonical namespace
    ///    (the default namespace from the SourceLoader, typically the filename stem).
    /// 2. Call instructions in each file are rewritten to use canonical namespaces.
    ///    E.g., if file A has `import "utils.rill" as helpers`, calls to `helpers::foo()`
    ///    are rewritten to `utils::foo()` (the canonical namespace from the loader).
    ///
    fn merge_ir(&self, diagnostics: &mut Diagnostics) -> ir::IrProgram {
        use std::collections::HashMap;

        let mut functions: Vec<ir::Function> = Vec::new();
        let mut constants: Vec<ir::ConstBinding> = Vec::new();
        let mut seen_names: HashMap<String, String> = HashMap::new();
        // Root-file globals occupy slots 0..N (the synthetic `__init__` stays
        // unqualified). Globals in imported files would need per-file slot
        // offsetting + init chaining in dependency order — deferred for now.
        let mut global_count = 0;

        for (idx, source) in self.sources.iter().enumerate() {
            let is_root = idx == 0;

            if is_root {
                global_count = source.ir.global_count;
            } else if source.ir.global_count > 0 {
                diagnostics.error_no_span(
                    diagnostics::DiagnosticCode::E400_DuplicateDefinition,
                    format!(
                        "file-scope globals in imported file '{}' are not yet supported",
                        source.canonical_id
                    ),
                );
            }

            // Build import alias → canonical namespace mapping for this file.
            // E.g., if this file has `import "utils.rill" as helpers`, then
            // helpers → utils (the canonical namespace from the loader).
            let mut alias_to_canonical: HashMap<String, String> = HashMap::new();
            for (imported_canonical_id, alias) in &source.import_aliases {
                if let Some(alias_name) = alias {
                    let canonical_ns = self
                        .default_namespaces
                        .get(imported_canonical_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    alias_to_canonical.insert(alias_name.clone(), canonical_ns);
                }
                // `as _` imports are handled in lower_parsed_sources() via merged_imports.
            }

            // Clone and rewrite call instructions to use canonical namespaces
            for func in &source.ir.functions {
                let mut func = func.clone();

                // Rewrite Call instructions in all blocks
                if !alias_to_canonical.is_empty() {
                    for block in &mut func.blocks {
                        for inst in &mut block.instructions {
                            if let ir::Instruction::Call { function, .. } = &mut inst.node
                                && let Some(ns) = &function.namespace
                                && let Some(canonical_ns) = alias_to_canonical.get(&ns.0)
                            {
                                function.namespace = Some(ast::Identifier(canonical_ns.clone()));
                            }
                        }
                    }
                }

                // Determine the function's qualified name in the merged IR
                let qname = if is_root {
                    func.name.to_string()
                } else {
                    let ns = self
                        .default_namespaces
                        .get(&source.canonical_id)
                        .map(|s| s.as_str())
                        .unwrap_or("unknown");
                    format!("{}::{}", ns, func.name)
                };

                if let Some(prev_source) = seen_names.get(&qname) {
                    diagnostics.error_no_span(
                        diagnostics::DiagnosticCode::E400_DuplicateDefinition,
                        format!(
                            "duplicate function `{}` (defined in '{}' and '{}')",
                            qname, prev_source, source.canonical_id,
                        ),
                    );
                    continue;
                }

                seen_names.insert(qname.clone(), source.canonical_id.clone());

                if !is_root {
                    func.name = ast::Identifier(qname);
                }
                functions.push(func);
            }

            constants.extend(source.ir.constants.iter().cloned());
        }

        ir::IrProgram {
            functions,
            constants,
            global_count,
        }
    }
}

// No Default impl for Compiler — requires a SourceLoader reference
