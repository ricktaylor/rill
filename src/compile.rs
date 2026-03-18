//! IR-to-Closure Compilation and Execution
//!
//! Compiles the SSA-form IR into closure-threaded code for execution.
//! Each IR instruction becomes a Rust closure that captures its operands
//! (slot offsets, resolved builtin function pointers, constant values).
//!
//! # Architecture
//!
//! ```text
//! IrProgram → compile_program() → CompiledProgram
//!                                      │
//!                                      ▼
//!                               execute() loop
//!                               ┌──────────────┐
//!                               │ select block  │ ← outer loop (iterative)
//!                               │ run steps     │ ← sequential closures
//!                               │ terminator    │ ← selects next block
//!                               └──────────────┘
//! ```
//!
//! # Design Notes
//!
//! - No per-instruction dispatch switch — closures ARE the instructions
//! - Builtins resolved once at compile time (no runtime HashMap lookup)
//! - VarIds mapped to stack slot offsets at compile time
//! - Loops use iterative block dispatch (no Rust stack growth)
//! - User function calls use recursive inline loops (bounded by VM stack limit)
//! - Future: tail-call optimization can convert recursive calls to jumps

use crate::builtins::{BuiltinImpl, BuiltinRegistry, ExecResult};
use crate::diagnostics::Diagnostics;
use crate::exec::{ExecError, Float, HeapVal, SeqState, VM, Value};
use crate::ir::opt::TypeAnalysis;
use crate::ir::{
    BasicBlock, BlockId, Function, Instruction, IntrinsicOp, IrProgram, Literal, MatchPattern,
    Terminator, VarId,
};
use crate::types::BaseType;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

// ============================================================================
// Compiled Types
// ============================================================================

/// A compiled program ready for execution.
pub struct CompiledProgram {
    functions: Vec<CompiledFunction>,
    /// Function name → index into `functions`
    pub(crate) func_index: HashMap<String, usize>,
    /// Warnings from compilation (unused functions, etc.)
    pub warnings: Diagnostics,
}

/// A compiled function — flat array of step closures with block offsets.
struct CompiledFunction {
    /// All step closures for all blocks, flattened into a single contiguous array.
    /// Block boundaries are recorded in `block_starts`.
    steps: Vec<Step>,
    /// Offset into `steps` where each block begins.
    /// `block_starts[i]` is the index of the first step in block i.
    block_starts: Vec<usize>,
    entry: usize, // index into block_starts
    frame_size: usize,
    param_count: usize,
}

/// A step closure. Captures operands, operates on VM.
/// Instructions return Continue; terminators return NextBlock/Return/Exit.
type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

/// Result of executing a step.
enum Action {
    /// Continue to the next step in this block
    Continue,
    /// Jump to another block
    NextBlock(usize),
    /// Return from function
    Return(Option<Value>),
    /// Hard exit to driver (diverging builtin)
    Exit(Value),
}

// ============================================================================
// Slot Mapping
// ============================================================================

/// Maps VarIds to stack slot offsets.
/// Slot 0 = Frame info. VarId(n) → slot n + 1.
fn slot(var: VarId) -> usize {
    var.0 as usize + 1
}

/// Maps BlockId to index in the compiled blocks array.
fn build_block_map(blocks: &[BasicBlock]) -> HashMap<BlockId, usize> {
    blocks
        .iter()
        .enumerate()
        .map(|(idx, b)| (b.id, idx))
        .collect()
}

// ============================================================================
// Compilation: IR → Closures
// ============================================================================

/// Compile an IR program into closure-threaded code.
///
/// Includes a link phase that resolves all function references at compile time
/// and emits diagnostics for undefined or unused functions.
pub fn compile_program(
    ir: &IrProgram,
    builtins: &BuiltinRegistry,
) -> Result<CompiledProgram, Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    // Build user function index
    let mut func_index: HashMap<String, usize> = HashMap::new();
    for (idx, ir_func) in ir.functions.iter().enumerate() {
        func_index.insert(ir_func.name.to_string(), idx);
    }

    // Link phase: resolve all Call references, track usage
    let mut used_functions: HashSet<usize> = HashSet::new();
    let link_map = link_functions(
        ir,
        builtins,
        &func_index,
        &mut used_functions,
        &mut diagnostics,
    );

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

    // TODO: Warn about unused functions once pub/priv distinction exists.
    // Currently all functions are potential entry points (callable by embedder),
    // so we can't know which are truly unused without #[export] or pub/priv.
    let _ = used_functions;

    // Compile functions to closures
    let mut compiled_functions = Vec::new();
    for ir_func in &ir.functions {
        match compile_function(ir_func, &link_map) {
            Ok(compiled) => compiled_functions.push(compiled),
            Err(_) => {
                diagnostics.error_no_span(
                    crate::diagnostics::DiagnosticCode::E500_UndefinedExternal,
                    format!("internal error compiling function `{}`", ir_func.name),
                );
                return Err(diagnostics);
            }
        }
    }

    let mut program = CompiledProgram {
        functions: compiled_functions,
        func_index,
        warnings: Diagnostics::new(),
    };

    // Attach warnings to the program for the caller
    program.warnings = diagnostics;

    Ok(program)
}

/// Resolution of a function call — determined at link time.
#[derive(Clone)]
enum CallTarget {
    /// Native builtin — function pointer resolved at compile time
    Builtin(crate::builtins::BuiltinFn),
    /// User-defined function — index into CompiledProgram.functions
    UserFunction(usize),
}

/// Map from qualified function name to its resolved target.
type LinkMap = HashMap<String, CallTarget>;

/// Link phase: resolve all function references and track usage.
fn link_functions(
    ir: &IrProgram,
    builtins: &BuiltinRegistry,
    func_index: &HashMap<String, usize>,
    used_functions: &mut HashSet<usize>,
    diagnostics: &mut Diagnostics,
) -> LinkMap {
    let mut link_map = LinkMap::new();

    // Pre-populate with all builtins
    for (name, def) in builtins.iter() {
        if let BuiltinImpl::Native(f) = &def.implementation {
            link_map.insert(name.clone(), CallTarget::Builtin(*f));
        }
    }

    // Pre-populate with all user functions
    for (name, &idx) in func_index {
        link_map.insert(name.clone(), CallTarget::UserFunction(idx));
    }

    // Walk all Call instructions and verify references resolve
    for ir_func in &ir.functions {
        for block in &ir_func.blocks {
            for inst in &block.instructions {
                if let Instruction::Call { function, .. } = &inst.node {
                    let qname = function.qualified_name();
                    if let Some(target) = link_map.get(&qname) {
                        // Track user function usage
                        if let CallTarget::UserFunction(idx) = target {
                            used_functions.insert(*idx);
                        }
                    } else {
                        diagnostics.error(
                            crate::diagnostics::DiagnosticCode::E500_UndefinedExternal,
                            inst.span,
                            format!("undefined function `{}`", qname),
                        );
                    }
                }
            }
        }
    }

    link_map
}

/// Metadata for a reference created by MakeRef — used by WriteRef at compile time
/// to determine how to emit the write-back.
struct RefMeta {
    base_var: VarId,
    key_slot: Option<usize>,
}

impl RefMeta {
    fn base_slot(&self) -> usize {
        slot(self.base_var)
    }
}

/// Build a map from MakeRef dest VarId → RefMeta for WriteRef resolution.
fn build_ref_map(blocks: &[BasicBlock]) -> HashMap<VarId, RefMeta> {
    let mut map = HashMap::new();
    for block in blocks {
        for inst in &block.instructions {
            if let Instruction::MakeRef { dest, base, key } = &inst.node {
                map.insert(
                    *dest,
                    RefMeta {
                        base_var: *base,
                        key_slot: key.map(slot),
                    },
                );
            }
        }
    }
    map
}

/// Build a map from VarId → constant UInt value for compile-time resolution
/// of Cast/Widen target type codes.
fn build_const_uint_map(blocks: &[BasicBlock]) -> HashMap<VarId, u64> {
    let mut map = HashMap::new();
    for block in blocks {
        for inst in &block.instructions {
            if let Instruction::Const {
                dest,
                value: Literal::UInt(n),
            } = &inst.node
            {
                map.insert(*dest, *n);
            }
        }
    }
    map
}

fn compile_function(func: &Function, link_map: &LinkMap) -> Result<CompiledFunction, ExecError> {
    let block_map = build_block_map(&func.blocks);
    let frame_size = 1 + func.locals.len(); // slot 0 = Frame, then locals

    // Collect MakeRef metadata for WriteRef resolution
    let ref_map = build_ref_map(&func.blocks);

    // Type analysis for specialization — when both operands of an arithmetic
    // op are provably the same type, the compiler emits a direct closure
    // instead of the 10-way type dispatch.
    let types = crate::ir::opt::analyze_types(func, None);

    // Definedness analysis for skipping None checks on provably-defined args
    let defs = crate::ir::opt::analyze_definedness(func, None);

    // Collect constant UInt values for Cast/Widen target resolution at compile time
    let const_uint_map = build_const_uint_map(&func.blocks);

    // First pass: compile all blocks, collecting phi metadata
    let mut blocks = Vec::new();
    #[allow(clippy::type_complexity)]
    let mut pending_phis: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
    // pending_phis: (dest_slot, join_block_idx, [(pred_block_idx, src_slot)])

    for ir_block in &func.blocks {
        let join_idx = block_map[&ir_block.id];

        // Collect phis from this block
        for inst in &ir_block.instructions {
            if let Instruction::Phi { dest, sources } = &inst.node {
                let d = slot(*dest);
                let compiled_sources: Vec<(usize, usize)> = sources
                    .iter()
                    .filter_map(|(block_id, var_id)| {
                        block_map.get(block_id).map(|&idx| (idx, slot(*var_id)))
                    })
                    .collect();

                // Skip identity phis (all sources are the dest slot)
                let all_same_as_dest =
                    !compiled_sources.is_empty() && compiled_sources.iter().all(|(_, s)| *s == d);
                if !compiled_sources.is_empty() && !all_same_as_dest {
                    pending_phis.push((d, join_idx, compiled_sources));
                }
            }
        }

        blocks.push(compile_block(
            ir_block,
            &block_map,
            link_map,
            &ref_map,
            &types,
            &defs,
            &const_uint_map,
        )?);
    }

    // Second pass: resolve phis by inserting copies into predecessor blocks.
    // Each phi source (pred_block, src_slot) → insert Copy(dest, src) before
    // the terminator (last element) of the predecessor block.
    for (dest_slot, _join_idx, sources) in pending_phis {
        for (pred_block_idx, src_slot) in sources {
            if src_slot != dest_slot {
                let d = dest_slot;
                let s = src_slot;
                let block = &mut blocks[pred_block_idx];
                let insert_pos = if block.is_empty() { 0 } else { block.len() - 1 };
                block.insert(
                    insert_pos,
                    Box::new(move |vm: &mut VM, _prog| {
                        if let Some(val) = vm.local(s).cloned() {
                            vm.set_local(d, val);
                        } else {
                            vm.set_local_uninit(d);
                        }
                        Ok(Action::Continue)
                    }),
                );
            }
        }
    }

    // Flatten blocks into a single contiguous step array with offsets.
    let mut steps: Vec<Step> = Vec::new();
    let mut block_starts: Vec<usize> = Vec::new();
    for block in blocks {
        block_starts.push(steps.len());
        steps.extend(block);
    }

    let entry = *block_map
        .get(&func.entry_block)
        .expect("entry block must exist");

    Ok(CompiledFunction {
        steps,
        block_starts,
        entry,
        frame_size,
        param_count: func.params.len(),
    })
}

fn compile_block(
    block: &BasicBlock,
    block_map: &HashMap<BlockId, usize>,
    link_map: &LinkMap,
    ref_map: &HashMap<VarId, RefMeta>,
    types: &TypeAnalysis,
    defs: &crate::ir::opt::DefinednessAnalysis,
    consts: &HashMap<VarId, u64>,
) -> Result<Vec<Step>, ExecError> {
    let mut steps: Vec<Step> = Vec::new();

    for spanned_inst in &block.instructions {
        match &spanned_inst.node {
            // Phis are handled in compile_function's second pass —
            // copies are inserted into predecessor blocks
            Instruction::Phi { .. } => {}
            inst => {
                if let Some(step) = compile_instruction(
                    inst, block_map, link_map, ref_map, types, defs, block.id, consts,
                )? {
                    steps.push(step);
                }
            }
        }
    }

    // Terminator is the last step in the block
    steps.push(compile_terminator(
        &block.terminator,
        block_map,
        types,
        defs,
        block.id,
    )?);

    Ok(steps)
}

#[allow(clippy::too_many_arguments)]
fn compile_instruction(
    inst: &Instruction,
    _block_map: &HashMap<BlockId, usize>,
    link_map: &LinkMap,
    ref_map: &HashMap<VarId, RefMeta>,
    types: &TypeAnalysis,
    defs: &crate::ir::opt::DefinednessAnalysis,
    block_id: BlockId,
    consts: &HashMap<VarId, u64>,
) -> Result<Option<Step>, ExecError> {
    Ok(Some(match inst {
        Instruction::Const { dest, value } => {
            let d = slot(*dest);
            // Pre-compute scalar values at compile time — no runtime match needed.
            // Only Text/Bytes require runtime heap allocation.
            match value {
                Literal::Bool(b) => {
                    let v = Value::Bool(*b);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::UInt(n) => {
                    let v = Value::UInt(*n);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::Int(n) => {
                    let v = Value::Int(*n);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::Float(f) => {
                    match Float::new(*f) {
                        Some(float) => {
                            let v = Value::Float(float);
                            Box::new(move |vm: &mut VM, _prog| {
                                vm.set_local(d, v.clone());
                                Ok(Action::Continue)
                            })
                        }
                        None => {
                            // NaN → undefined
                            Box::new(move |vm: &mut VM, _prog| {
                                vm.set_local_uninit(d);
                                Ok(Action::Continue)
                            })
                        }
                    }
                }
                Literal::Text(s) => {
                    // Intern: allocate on first execution, reuse Rc clone after.
                    let text = s.clone();
                    let cache = std::cell::RefCell::new(None);
                    Box::new(move |vm: &mut VM, _prog| {
                        if cache.borrow().is_none() {
                            let v = Value::Text(HeapVal::new(text.clone(), vm.heap())?);
                            *cache.borrow_mut() = Some(v);
                        }
                        let val = cache.borrow().as_ref().unwrap().clone();
                        vm.set_local(d, val);
                        Ok(Action::Continue)
                    })
                }
                Literal::Bytes(b) => {
                    let bytes = b.clone();
                    let cache = std::cell::RefCell::new(None);
                    Box::new(move |vm: &mut VM, _prog| {
                        if cache.borrow().is_none() {
                            let v = Value::Bytes(HeapVal::new(bytes.clone(), vm.heap())?);
                            *cache.borrow_mut() = Some(v);
                        }
                        let val = cache.borrow().as_ref().unwrap().clone();
                        vm.set_local(d, val);
                        Ok(Action::Continue)
                    })
                }
            }
        }

        Instruction::Copy { dest, src } => {
            let d = slot(*dest);
            let s = slot(*src);
            if defs.get_at_exit(block_id, *src) == crate::ir::opt::Definedness::Defined {
                Box::new(move |vm: &mut VM, _prog| {
                    let val = vm.local(s).unwrap().clone();
                    vm.set_local(d, val);
                    Ok(Action::Continue)
                })
            } else {
                Box::new(move |vm: &mut VM, _prog| {
                    if let Some(val) = vm.local(s).cloned() {
                        vm.set_local(d, val);
                    }
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::Undefined { dest } => {
            let d = slot(*dest);
            Box::new(move |vm: &mut VM, _prog| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })
        }

        Instruction::Index { dest, base, key } => {
            let d = slot(*dest);
            let b = slot(*base);
            let k = slot(*key);

            // Specialize based on known base type
            let base_type = types.get_at_exit(block_id, *base).filter(|t| t.is_single());

            if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Some(Value::Array(arr)), Some(Value::UInt(idx))) => {
                            arr.get(*idx as usize).cloned()
                        }
                        (Some(Value::Array(arr)), Some(Value::Int(idx))) if *idx >= 0 => {
                            arr.get(*idx as usize).cloned()
                        }
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local_uninit(d),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Some(Value::Map(map)), Some(key_val)) => map.get(key_val).cloned(),
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local_uninit(d),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Text)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Some(Value::Text(s)), Some(Value::UInt(idx))) => {
                            s.chars().nth(*idx as usize).map(|c| Value::UInt(c as u64))
                        }
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local_uninit(d),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Bytes)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Some(Value::Bytes(bytes)), Some(Value::UInt(idx))) => bytes
                            .get(*idx as usize)
                            .map(|byte| Value::UInt(*byte as u64)),
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local_uninit(d),
                    }
                    Ok(Action::Continue)
                })
            } else {
                // Unknown base: full runtime dispatch
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Some(base_val), Some(key_val)) => index_value(base_val, key_val),
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local_uninit(d),
                    }
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::SetIndex { base, key, value } => {
            let b = slot(*base);
            let k = slot(*key);
            let v = slot(*value);

            // Specialize based on known base type
            let base_type = types.get_at_exit(block_id, *base).filter(|t| t.is_single());

            if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                // Array: key is UInt index
                Box::new(move |vm: &mut VM, _prog| {
                    if let (Some(Value::UInt(idx)), Some(new_val)) =
                        (vm.local(k), vm.local(v).cloned())
                    {
                        let _ = vm.set_array_elem(vm.bp() + b, *idx as usize, new_val);
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                // Map: any key type
                Box::new(move |vm: &mut VM, _prog| {
                    if let (Some(key_val), Some(new_val)) =
                        (vm.local(k).cloned(), vm.local(v).cloned())
                    {
                        let _ = vm.set_map_entry(vm.bp() + b, key_val, new_val);
                    }
                    Ok(Action::Continue)
                })
            } else {
                // Unknown base: dispatch on key type at runtime
                Box::new(move |vm: &mut VM, _prog| {
                    if let (Some(key_val), Some(new_val)) =
                        (vm.local(k).cloned(), vm.local(v).cloned())
                    {
                        match &key_val {
                            Value::UInt(idx) => {
                                let _ = vm.set_array_elem(vm.bp() + b, *idx as usize, new_val);
                            }
                            _ => {
                                let _ = vm.set_map_entry(vm.bp() + b, key_val, new_val);
                            }
                        }
                    }
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let d = slot(*dest);
            let arg_slots: Vec<(usize, bool)> =
                args.iter().map(|a| (slot(a.value), a.by_ref)).collect();
            let func_name = function.qualified_name();

            // Resolve via link map (all references verified at link time)
            match link_map.get(&func_name).cloned() {
                Some(CallTarget::Builtin(f)) => {
                    let argc = arg_slots.len();
                    Box::new(move |vm: &mut VM, _prog| {
                        // Set up frame for builtin (same convention as user functions)
                        let caller_bp = vm.bp();
                        let frame_size = 1 + argc; // slot 0 = Frame, slots 1..=N = args
                        vm.call(frame_size, None)?;

                        // Copy args from caller's slots into builtin's frame
                        for (i, (s, _by_ref)) in arg_slots.iter().enumerate() {
                            if let Some(val) = vm.get(caller_bp + s).cloned() {
                                vm.set_local(i + 1, val);
                            }
                        }

                        // Call builtin — reads args via vm.arg(i)
                        let result = f(vm, argc);
                        vm.ret();

                        match result? {
                            ExecResult::Return(Some(val)) => vm.set_local(d, val),
                            ExecResult::Return(None) => vm.set_local_uninit(d),
                            ExecResult::Exit(val) => return Ok(Action::Exit(val)),
                        }
                        Ok(Action::Continue)
                    })
                }
                Some(CallTarget::UserFunction(func_idx)) => {
                    Box::new(move |vm: &mut VM, prog: &CompiledProgram| {
                        let func = &prog.functions[func_idx];

                        // Save caller's bp, then set up callee frame
                        let caller_bp = vm.bp();
                        vm.call(func.frame_size, None)?;

                        // Copy args directly from caller's slots into callee's param slots.
                        // No intermediate Vec allocation.
                        for (i, (s, _by_ref)) in arg_slots.iter().enumerate() {
                            if i < func.param_count
                                && let Some(val) = vm.get(caller_bp + s).cloned()
                            {
                                vm.set_local(i + 1, val);
                            }
                        }

                        // Execute callee: inline loop (same as execute_function)
                        let mut pc = func.block_starts[func.entry];
                        let result = loop {
                            match (func.steps[pc])(vm, prog)? {
                                Action::Continue => pc += 1,
                                Action::NextBlock(idx) => pc = func.block_starts[idx],
                                Action::Return(val) => {
                                    vm.ret();
                                    break val;
                                }
                                Action::Exit(val) => {
                                    vm.ret();
                                    return Ok(Action::Exit(val));
                                }
                            }
                        };

                        match result {
                            Some(val) => vm.set_local(d, val),
                            None => vm.set_local_uninit(d),
                        }
                        Ok(Action::Continue)
                    })
                }
                None => {
                    // Unresolved — link phase should have caught this.
                    // Emit undefined as fallback.
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local_uninit(d);
                        Ok(Action::Continue)
                    })
                }
            }
        }

        Instruction::Intrinsic { dest, op, args } => {
            let d = slot(*dest);
            let op = *op;
            let arg_slots: Vec<usize> = args.iter().map(|v| slot(*v)).collect();

            // Try type-specialized compilation for binary arithmetic.
            // If both operands are provably the same single numeric type,
            // emit a direct closure that skips the runtime type dispatch.
            if let Some(specialized) =
                try_specialize_binary(op, &arg_slots, d, args, types, block_id)
            {
                return Ok(Some(specialized));
            }

            // Try type-specialized compilation for Cast and Widen.
            // Target is always a compile-time constant, so these always
            // succeed — eliminating target slot reads and target dispatch.
            if let Some(specialized) = try_specialize_cast(
                op, &arg_slots, d, args, types, block_id, consts,
            )
            .or_else(|| try_specialize_widen(op, &arg_slots, d, args, types, block_id, consts))
            {
                return Ok(Some(specialized));
            }

            // Dispatch on op at compile time so each closure goes directly
            // to its operation's code — no runtime `match op` in exec_intrinsic.
            // If all args are provably defined, skip Option unwraps at runtime.
            let all_defined = args
                .iter()
                .all(|v| defs.get_at_exit(block_id, *v) == crate::ir::opt::Definedness::Defined);
            compile_intrinsic_dispatch(op, arg_slots, d, all_defined)
        }

        Instruction::MakeRef { dest, base, key } => {
            let d = slot(*dest);
            let b = slot(*base);
            match key {
                Some(k) => {
                    // Element reference: read base[key] into dest
                    // Specialize based on known base type (same as Index)
                    let k_slot = slot(*k);
                    let base_type = types.get_at_exit(block_id, *base).filter(|t| t.is_single());

                    if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                        Box::new(move |vm: &mut VM, _prog| {
                            let result = match (vm.local(b), vm.local(k_slot)) {
                                (Some(Value::Array(arr)), Some(Value::UInt(idx))) => {
                                    arr.get(*idx as usize).cloned()
                                }
                                (Some(Value::Array(arr)), Some(Value::Int(idx))) if *idx >= 0 => {
                                    arr.get(*idx as usize).cloned()
                                }
                                _ => None,
                            };
                            match result {
                                Some(val) => vm.set_local(d, val),
                                None => vm.set_local_uninit(d),
                            }
                            Ok(Action::Continue)
                        })
                    } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                        Box::new(move |vm: &mut VM, _prog| {
                            let result = match (vm.local(b), vm.local(k_slot)) {
                                (Some(Value::Map(map)), Some(key_val)) => map.get(key_val).cloned(),
                                _ => None,
                            };
                            match result {
                                Some(val) => vm.set_local(d, val),
                                None => vm.set_local_uninit(d),
                            }
                            Ok(Action::Continue)
                        })
                    } else {
                        // Unknown base: full runtime dispatch
                        Box::new(move |vm: &mut VM, _prog| {
                            let result = match (vm.local(b), vm.local(k_slot)) {
                                (Some(base_val), Some(key_val)) => index_value(base_val, key_val),
                                _ => None,
                            };
                            match result {
                                Some(val) => vm.set_local(d, val),
                                None => vm.set_local_uninit(d),
                            }
                            Ok(Action::Continue)
                        })
                    }
                }
                None => {
                    // Whole-value reference: create a Slot::Ref to base's slot
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local_ref(d, vm.bp() + b);
                        Ok(Action::Continue)
                    })
                }
            }
        }

        Instruction::WriteRef { ref_var, value } => {
            let v = slot(*value);
            // Look up the MakeRef that created this ref_var to find
            // the base and key slots for write-back.
            if let Some(meta) = ref_map.get(ref_var) {
                let base_slot = meta.base_slot();
                match meta.key_slot {
                    Some(key_slot) => {
                        // Element write-back: specialize based on known base type
                        let base_type = types
                            .get_at_exit(block_id, meta.base_var)
                            .filter(|t| t.is_single());

                        if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                            // Array: key is UInt index
                            Box::new(move |vm: &mut VM, _prog| {
                                if let (Some(Value::UInt(idx)), Some(new_val)) =
                                    (vm.local(key_slot), vm.local(v).cloned())
                                {
                                    let _ = vm.set_array_elem(
                                        vm.bp() + base_slot,
                                        *idx as usize,
                                        new_val,
                                    );
                                }
                                Ok(Action::Continue)
                            })
                        } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                            // Map: any key type
                            Box::new(move |vm: &mut VM, _prog| {
                                if let (Some(key_val), Some(new_val)) =
                                    (vm.local(key_slot).cloned(), vm.local(v).cloned())
                                {
                                    let _ = vm.set_map_entry(vm.bp() + base_slot, key_val, new_val);
                                }
                                Ok(Action::Continue)
                            })
                        } else {
                            // Unknown base: dispatch on key type at runtime
                            Box::new(move |vm: &mut VM, _prog| {
                                if let (Some(key_val), Some(new_val)) =
                                    (vm.local(key_slot).cloned(), vm.local(v).cloned())
                                {
                                    match &key_val {
                                        Value::UInt(idx) => {
                                            let _ = vm.set_array_elem(
                                                vm.bp() + base_slot,
                                                *idx as usize,
                                                new_val,
                                            );
                                        }
                                        _ => {
                                            let _ = vm.set_map_entry(
                                                vm.bp() + base_slot,
                                                key_val,
                                                new_val,
                                            );
                                        }
                                    }
                                }
                                Ok(Action::Continue)
                            })
                        }
                    }
                    None => {
                        // Whole-value write-back: write to base's slot directly
                        Box::new(move |vm: &mut VM, _prog| {
                            if let Some(val) = vm.local(v).cloned() {
                                vm.set_local(base_slot, val);
                            }
                            Ok(Action::Continue)
                        })
                    }
                }
            } else {
                // MakeRef not found — shouldn't happen in well-formed IR.
                // No-op fallback.
                return Ok(None);
            }
        }

        Instruction::Drop { .. } => {
            // No-op for now — slots are reclaimed when frame is popped
            return Ok(None);
        }

        // Phi is handled separately in compile_block
        Instruction::Phi { .. } => return Ok(None),
    }))
}

fn compile_terminator(
    term: &Terminator,
    block_map: &HashMap<BlockId, usize>,
    types: &TypeAnalysis,
    defs: &crate::ir::opt::DefinednessAnalysis,
    block_id: BlockId,
) -> Result<Step, ExecError> {
    Ok(match term {
        Terminator::Jump { target } => {
            let idx = block_map[target];
            Box::new(move |_vm, _prog| Ok(Action::NextBlock(idx)))
        }

        Terminator::If {
            condition,
            then_target,
            else_target,
            ..
        } => {
            let cond_slot = slot(*condition);
            let then_idx = block_map[then_target];
            let else_idx = block_map[else_target];

            let cond_type = types
                .get_at_exit(block_id, *condition)
                .copied()
                .unwrap_or(crate::types::TypeSet::all());
            let cond_def = defs.get_at_exit(block_id, *condition);

            // Non-Bool conditions should have been folded to Jump(else) by the
            // optimizer's fold_non_bool_conditions pass.
            debug_assert!(
                cond_type.contains(BaseType::Bool) || cond_type.is_empty(),
                "If condition with non-Bool type {:?} should have been folded by optimizer",
                cond_type
            );

            // Provably Bool and Defined → skip null + type checks
            if cond_type.is_single()
                && cond_type.contains(BaseType::Bool)
                && cond_def == crate::ir::opt::Definedness::Defined
            {
                return Ok(Box::new(move |vm: &mut VM, _prog| {
                    let is_true = match vm.local(cond_slot).unwrap() {
                        Value::Bool(b) => *b,
                        _ => unreachable!(),
                    };
                    Ok(Action::NextBlock(if is_true { then_idx } else { else_idx }))
                }));
            }

            Box::new(move |vm: &mut VM, _prog| {
                let is_true = vm
                    .local(cond_slot)
                    .map(|v| matches!(v, Value::Bool(true)))
                    .unwrap_or(false);
                Ok(Action::NextBlock(if is_true { then_idx } else { else_idx }))
            })
        }

        Terminator::Match {
            value,
            arms,
            default,
            ..
        } => {
            let val_slot = slot(*value);
            let default_idx = block_map[default];
            compile_match(val_slot, arms, default_idx, block_map)
        }

        Terminator::Guard {
            value,
            defined,
            undefined,
            ..
        } => {
            // Guards with known definedness should have been folded to Jump
            // by the optimizer's eliminate_guards pass.
            debug_assert!(
                defs.get_at_exit(block_id, *value) == crate::ir::opt::Definedness::MaybeDefined,
                "Guard on {:?} with definedness {:?} should have been eliminated by optimizer",
                value,
                defs.get_at_exit(block_id, *value)
            );

            let val_slot = slot(*value);
            let def_idx = block_map[defined];
            let undef_idx = block_map[undefined];
            Box::new(move |vm: &mut VM, _prog| {
                let is_defined = vm.local(val_slot).is_some();
                Ok(Action::NextBlock(if is_defined {
                    def_idx
                } else {
                    undef_idx
                }))
            })
        }

        Terminator::Return { value } => {
            let val_slot = value.map(slot);
            Box::new(move |vm: &mut VM, _prog| {
                let val = val_slot.and_then(|s| vm.local(s).cloned());
                Ok(Action::Return(val))
            })
        }

        Terminator::Exit { value } => {
            let val_slot = slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                let val = vm.local(val_slot).cloned().unwrap_or(Value::UInt(0));
                Ok(Action::Exit(val))
            })
        }

        Terminator::Unreachable => Box::new(|_vm, _prog| Ok(Action::Return(None))),
    })
}

// ============================================================================
// Match Compilation
// ============================================================================

/// Compile a Match terminator, specializing based on arm count and pattern type.
///
/// - Single-arm type match: direct `base_type()` comparison (most common case from if-let)
/// - Single-arm literal: direct value comparison
/// - Single-arm array/array-min: direct length check
/// - Multi-arm: pre-compiled predicate closures (no MatchPattern dispatch at runtime)
fn compile_match(
    val_slot: usize,
    arms: &[(MatchPattern, BlockId)],
    default_idx: usize,
    block_map: &HashMap<BlockId, usize>,
) -> Step {
    if arms.len() == 1 {
        // Single-arm fast path — inline the pattern test directly
        let target_idx = block_map[&arms[0].1];
        return compile_single_arm_match(val_slot, &arms[0].0, target_idx, default_idx);
    }

    // Multi-arm: pre-compile each pattern into a predicate closure
    #[allow(clippy::type_complexity)]
    let compiled_arms: Vec<(Box<dyn Fn(&Value) -> bool>, usize)> = arms
        .iter()
        .map(|(pat, target)| (compile_match_predicate(pat), block_map[target]))
        .collect();

    Box::new(move |vm: &mut VM, _prog| {
        if let Some(val) = vm.local(val_slot) {
            for (predicate, target_idx) in &compiled_arms {
                if predicate(val) {
                    return Ok(Action::NextBlock(*target_idx));
                }
            }
        }
        Ok(Action::NextBlock(default_idx))
    })
}

/// Compile a single-arm Match into a direct test — no Vec, no predicate dispatch.
fn compile_single_arm_match(
    val_slot: usize,
    pattern: &MatchPattern,
    target_idx: usize,
    default_idx: usize,
) -> Step {
    match pattern {
        MatchPattern::Type(base_type) => {
            let ty = *base_type;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = vm.local(val_slot).is_some_and(|v| v.base_type() == ty);
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::Literal(lit) => {
            let pred = compile_match_predicate(&MatchPattern::Literal(lit.clone()));
            Box::new(move |vm: &mut VM, _prog| {
                let matched = vm.local(val_slot).is_some_and(&pred);
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::Array(len) => {
            let expected = *len;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = vm
                    .local(val_slot)
                    .is_some_and(|v| matches!(v, Value::Array(a) if a.len() == expected));
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::ArrayMin(min) => {
            let expected = *min;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = vm
                    .local(val_slot)
                    .is_some_and(|v| matches!(v, Value::Array(a) if a.len() >= expected));
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
    }
}

/// Pre-compile a MatchPattern into a predicate closure for multi-arm dispatch.
/// The MatchPattern enum is resolved at compile time — the returned closure
/// does only the value-level test with no pattern variant dispatch.
fn compile_match_predicate(pattern: &MatchPattern) -> Box<dyn Fn(&Value) -> bool> {
    match pattern {
        MatchPattern::Type(base_type) => {
            let ty = *base_type;
            Box::new(move |v| v.base_type() == ty)
        }
        MatchPattern::Literal(lit) => match lit {
            Literal::Bool(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Bool(b) if *b == e))
            }
            Literal::UInt(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::UInt(n) if *n == e))
            }
            Literal::Int(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Int(n) if *n == e))
            }
            Literal::Float(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Float(f) if f.get() == e))
            }
            Literal::Text(expected) => {
                let e = expected.clone();
                Box::new(move |v| matches!(v, Value::Text(s) if **s == *e))
            }
            Literal::Bytes(expected) => {
                let e = expected.clone();
                Box::new(move |v| matches!(v, Value::Bytes(b) if **b == *e))
            }
        },
        MatchPattern::Array(len) => {
            let expected = *len;
            Box::new(move |v| matches!(v, Value::Array(a) if a.len() == expected))
        }
        MatchPattern::ArrayMin(min) => {
            let expected = *min;
            Box::new(move |v| matches!(v, Value::Array(a) if a.len() >= expected))
        }
    }
}

// ============================================================================
// Value Indexing (runtime)
// ============================================================================

fn index_value(base: &Value, key: &Value) -> Option<Value> {
    match (base, key) {
        (Value::Array(arr), Value::UInt(idx)) => arr.get(*idx as usize).cloned(),
        (Value::Array(arr), Value::Int(idx)) if *idx >= 0 => arr.get(*idx as usize).cloned(),
        (Value::Map(map), key) => map.get(key).cloned(),
        (Value::Text(s), Value::UInt(idx)) => {
            s.chars().nth(*idx as usize).map(|c| {
                // Text indexing returns a single-char string — but we need HeapVal.
                // For now, return UInt of the char code. TODO: return Text properly.
                Value::UInt(c as u64)
            })
        }
        (Value::Bytes(b), Value::UInt(idx)) => {
            b.get(*idx as usize).map(|byte| Value::UInt(*byte as u64))
        }
        _ => None,
    }
}

// ============================================================================
// Execution
// ============================================================================

/// Execute a named function (convenience — does HashMap lookup).
///
/// Args should be pushed onto the VM stack before calling:
/// ```ignore
/// vm.push(Value::UInt(42))?;
/// let result = execute(&program, &mut vm, "func", 1)?;
/// ```
pub fn execute(
    program: &CompiledProgram,
    vm: &mut VM,
    func_name: &str,
    argc: usize,
) -> Result<Option<Value>, ExecError> {
    let func_idx = *program
        .func_index
        .get(func_name)
        .ok_or(ExecError::StackOverflow)?; // TODO: proper "function not found" error
    execute_by_index(program, vm, func_idx, argc)
}

/// Execute a function by resolved index (no lookup — hot path).
///
/// Args should be pushed onto the VM stack before calling.
pub fn execute_by_index(
    program: &CompiledProgram,
    vm: &mut VM,
    func_idx: usize,
    argc: usize,
) -> Result<Option<Value>, ExecError> {
    let func = &program.functions[func_idx];

    // Adopt pushed args into the call frame (zero allocation)
    vm.call_with_args(func.frame_size, argc)?;

    // Execute: single flat loop with program counter
    let mut pc = func.block_starts[func.entry];

    loop {
        match (func.steps[pc])(vm, program)? {
            Action::Continue => pc += 1,
            Action::NextBlock(idx) => pc = func.block_starts[idx],
            Action::Return(val) => {
                vm.ret();
                return Ok(val);
            }
            Action::Exit(_val) => {
                vm.ret();
                return Ok(None); // TODO: propagate exit to driver
            }
        }
    }
}

// ============================================================================
// Intrinsic Runtime Execution
// ============================================================================

/// Try to emit a type-specialized closure for a binary arithmetic intrinsic.
///
/// Consults TypeAnalysis: if both operands are provably a single numeric type
/// and they match, emits a direct closure that skips the 10-way runtime
/// type dispatch. Returns `None` to fall back to `compile_intrinsic_dispatch`.
fn try_specialize_binary(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
) -> Option<Step> {
    // Only specialize binary arithmetic and comparison
    if args.len() != 2 {
        return None;
    }
    if !matches!(
        op,
        IntrinsicOp::Add
            | IntrinsicOp::Sub
            | IntrinsicOp::Mul
            | IntrinsicOp::Div
            | IntrinsicOp::Mod
            | IntrinsicOp::Lt
            | IntrinsicOp::Eq
    ) {
        return None;
    }

    let a_type = types.get_at_exit(block_id, args[0])?;
    let b_type = types.get_at_exit(block_id, args[1])?;

    // Both must be single and the same type
    if !a_type.is_single() || !b_type.is_single() || a_type != b_type {
        return None;
    }

    let a = arg_slots[0];
    let b = arg_slots[1];
    let d = dest_slot;

    // Determine the single type
    if a_type.contains(BaseType::UInt) {
        Some(specialize_uint(op, a, b, d))
    } else if a_type.contains(BaseType::Int) {
        Some(specialize_int(op, a, b, d))
    } else if a_type.contains(BaseType::Float) {
        Some(specialize_float(op, a, b, d))
    } else {
        None
    }
}

/// Try to emit a type-specialized closure for a Cast intrinsic.
///
/// Consults TypeAnalysis for the source type and the const map for the target
/// type code. Three levels of specialization:
///
/// 1. Source type + target both known → fully specialized (identity copy or
///    single direct conversion, zero dispatch at runtime)
/// 2. Target known, source unknown → target-specialized closure that only
///    dispatches on source value type (eliminates target slot read + target match)
/// 3. Neither known → falls through to `compile_intrinsic_dispatch`
fn try_specialize_cast(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
    consts: &HashMap<VarId, u64>,
) -> Option<Step> {
    if op != IntrinsicOp::Cast || args.len() != 2 {
        return None;
    }

    let target = *consts.get(&args[1])?;
    let src = arg_slots[0];
    let d = dest_slot;

    // Check if source type is known
    let src_code = types
        .get_at_exit(block_id, args[0])
        .filter(|t| t.is_single())
        .and_then(|t| {
            if t.contains(BaseType::UInt) {
                Some(1u64)
            } else if t.contains(BaseType::Int) {
                Some(2u64)
            } else if t.contains(BaseType::Float) {
                Some(3u64)
            } else {
                None
            }
        });

    if let Some(src_code) = src_code {
        // === Level 1: both source type and target known ===

        // Identity casts should have been replaced with Copy by the
        // optimizer's elide_identity_casts pass.
        debug_assert!(
            src_code != target,
            "Identity Cast (src={}, target={}) should have been elided by optimizer",
            src_code,
            target
        );

        // Fully specialized conversion — no dispatch at runtime
        return match (src_code, target) {
            (1, 2) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                vm.set_local(d, Value::Int(n as i64));
                Ok(Action::Continue)
            })),
            (1, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            (2, 1) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                vm.set_local(d, Value::UInt(n as u64));
                Ok(Action::Continue)
            })),
            (2, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            (3, _) => Some(Box::new(move |vm: &mut VM, _| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })),
            _ => None,
        };
    }

    // === Level 2: target known, source type unknown ===
    // Emit a target-specific closure — eliminates target slot read and
    // target match; only source value dispatch remains.
    Some(match target {
        1 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Some(Value::UInt(*n)),
                Some(Value::Int(n)) => Some(Value::UInt(*n as u64)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        2 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Some(Value::Int(*n as i64)),
                Some(Value::Int(n)) => Some(Value::Int(*n)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        3 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Int(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Float(f)) => Some(Value::Float(*f)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        _ => return None,
    })
}

/// Try to emit a target-specialized closure for a Widen intrinsic.
///
/// Same approach as `try_specialize_cast`: the target type code is always a
/// compile-time constant. Unlike Cast, Widen is overflow-checked (UInt→Int
/// fails if value > i64::MAX).
fn try_specialize_widen(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
    consts: &HashMap<VarId, u64>,
) -> Option<Step> {
    if op != IntrinsicOp::Widen || args.len() != 2 {
        return None;
    }

    let target = *consts.get(&args[1])?;
    let src = arg_slots[0];
    let d = dest_slot;

    // Check if source type is known
    let src_code = types
        .get_at_exit(block_id, args[0])
        .filter(|t| t.is_single())
        .and_then(|t| {
            if t.contains(BaseType::UInt) {
                Some(1u64)
            } else if t.contains(BaseType::Int) {
                Some(2u64)
            } else if t.contains(BaseType::Float) {
                Some(3u64)
            } else {
                None
            }
        });

    if let Some(src_code) = src_code {
        // === Fully specialized: source type + target both known ===

        // Identity widens should have been replaced with Copy by the
        // optimizer's elide_identity_casts pass.
        debug_assert!(
            src_code != target,
            "Identity Widen (src={}, target={}) should have been elided by optimizer",
            src_code,
            target
        );

        return match (src_code, target) {
            // UInt → Int: overflow-checked
            (1, 2) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                if n > i64::MAX as u64 {
                    vm.set_local_uninit(d);
                } else {
                    vm.set_local(d, Value::Int(n as i64));
                }
                Ok(Action::Continue)
            })),
            // UInt → Float
            (1, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            // Int → Float
            (2, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            _ => Some(Box::new(move |vm: &mut VM, _| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })),
        };
    }

    // === Target known, source unknown ===
    Some(match target {
        2 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => {
                    if *n > i64::MAX as u64 {
                        None
                    } else {
                        Some(Value::Int(*n as i64))
                    }
                }
                Some(Value::Int(n)) => Some(Value::Int(*n)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        3 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Int(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Float(f)) => Some(Value::Float(*f)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        _ => return None,
    })
}

// Type-extraction helpers for specialized closures.
// These use expect() rather than silent fallback — the type analysis has
// proven the types, so a mismatch is a compiler bug that should surface
// immediately during testing.

fn expect_uint(vm: &VM, slot: usize) -> u64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::UInt(n) => *n,
        other => panic!("specialized: expected UInt, got {:?}", other),
    }
}

fn expect_int(vm: &VM, slot: usize) -> i64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::Int(n) => *n,
        other => panic!("specialized: expected Int, got {:?}", other),
    }
}

fn expect_float(vm: &VM, slot: usize) -> f64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::Float(f) => f.get(),
        other => panic!("specialized: expected Float, got {:?}", other),
    }
}

/// Emit a UInt-specialized closure for a binary op.
fn specialize_uint(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_add(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_sub(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_mul(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_div(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_rem(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Emit an Int-specialized closure for a binary op.
fn specialize_int(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_add(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_sub(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_mul(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_div(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_rem(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Emit a Float-specialized closure for a binary op.
fn specialize_float(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x + y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x - y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x * y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x / y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x % y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Compile-time dispatch: match on the IntrinsicOp and return a closure
/// specific to that operation. Eliminates the runtime `match op` that
/// `exec_intrinsic` would perform on every execution.
fn compile_intrinsic_dispatch(
    op: IntrinsicOp,
    arg_slots: Vec<usize>,
    d: usize,
    all_defined: bool,
) -> Step {
    // Helper: wrap exec body in the standard result-to-slot pattern
    macro_rules! emit {
        ($body:expr) => {
            Box::new(move |vm: &mut VM, _prog| {
                let result: Option<Value> = $body(vm);
                match result {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        };
    }
    // Helper for operations that need ExecError propagation
    macro_rules! emit_try {
        ($body:expr) => {
            Box::new(move |vm: &mut VM, _prog| {
                match $body(vm)? {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        };
    }

    // Helpers for binary/unary ops: when all_defined, skip the Option unwrap
    // and pass &Value directly; otherwise gate on Some first.
    macro_rules! emit_binary {
        ($op_fn:ident) => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let a = vm.local(arg_slots[0]).unwrap();
                    let b = vm.local(arg_slots[1]).unwrap();
                    $op_fn(a, b)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match (vm.local(arg_slots[0]), vm.local(arg_slots[1])) {
                        (Some(a), Some(b)) => $op_fn(a, b),
                        _ => None,
                    }
                })
            }
        };
    }
    macro_rules! emit_unary {
        ($op_fn:ident) => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let a = vm.local(arg_slots[0]).unwrap();
                    $op_fn(a)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match vm.local(arg_slots[0]) {
                        Some(a) => $op_fn(a),
                        None => None,
                    }
                })
            }
        };
    }

    match op {
        IntrinsicOp::Add => emit_binary!(exec_add),
        IntrinsicOp::Sub => emit_binary!(exec_sub),
        IntrinsicOp::Mul => emit_binary!(exec_mul),
        IntrinsicOp::Div => emit_binary!(exec_div),
        IntrinsicOp::Mod => emit_binary!(exec_mod),
        IntrinsicOp::Neg => emit_unary!(exec_neg),
        IntrinsicOp::Eq => emit_binary!(exec_eq),
        IntrinsicOp::Lt => emit_binary!(exec_lt),
        IntrinsicOp::Not => emit_unary!(exec_not),
        IntrinsicOp::BitAnd => emit_binary!(exec_bitand),
        IntrinsicOp::BitOr => emit_binary!(exec_bitor),
        IntrinsicOp::BitXor => emit_binary!(exec_bitxor),
        IntrinsicOp::BitNot => emit_unary!(exec_bitnot),
        IntrinsicOp::Shl => emit_binary!(exec_shl),
        IntrinsicOp::Shr => emit_binary!(exec_shr),
        IntrinsicOp::BitTest => emit_binary!(exec_bittest),
        IntrinsicOp::BitSet => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let x = vm.local(arg_slots[0]).unwrap();
                    let b = vm.local(arg_slots[1]).unwrap();
                    let v = vm.local(arg_slots[2]).unwrap();
                    exec_bitset(x, b, v)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match (
                        vm.local(arg_slots[0]),
                        vm.local(arg_slots[1]),
                        vm.local(arg_slots[2]),
                    ) {
                        (Some(x), Some(b), Some(v)) => exec_bitset(x, b, v),
                        _ => None,
                    }
                })
            }
        }
        IntrinsicOp::Len => emit_unary!(exec_len),
        IntrinsicOp::MakeArray => emit_try!(|vm: &mut VM| { exec_make_array(&arg_slots, vm) }),
        IntrinsicOp::MakeMap => emit_try!(|vm: &mut VM| { exec_make_map(&arg_slots, vm) }),
        IntrinsicOp::MakeSeq => emit!(|vm: &mut VM| { exec_make_seq(&arg_slots, vm) }),
        IntrinsicOp::ArraySeq => emit!(|vm: &mut VM| { exec_array_seq(&arg_slots, vm) }),
        IntrinsicOp::SeqNext => Box::new(move |vm: &mut VM, _prog| {
            match vm.seq_next(vm.bp() + arg_slots[0])? {
                Some(val) => vm.set_local(d, val),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Collect => Box::new(move |vm: &mut VM, _prog| {
            match vm.seq_collect(vm.bp() + arg_slots[0])? {
                Some(val) => vm.set_local(d, val),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Widen => emit!(|vm: &mut VM| { exec_widen(&arg_slots, vm) }),
        IntrinsicOp::Cast => emit!(|vm: &mut VM| { exec_cast(&arg_slots, vm) }),
    }
}

// ========================================================================
// Per-operation functions for compile-time dispatch
// Each takes &Value directly (no Option wrapper, no slot lookup, no op dispatch).
// The Option handling is done at the call site in compile_intrinsic_dispatch:
// - all_defined=true: unwrap() then call (skips None check entirely)
// - all_defined=false: gate on Some first, call only if all present
// ========================================================================

fn exec_add(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_add(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_add(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() + b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_add(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_add(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
        _ => None,
    }
}

fn exec_sub(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_sub(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_sub(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() - b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_sub(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_sub(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
        _ => None,
    }
}

fn exec_mul(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_mul(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_mul(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() * b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_mul(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_mul(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
        _ => None,
    }
}

fn exec_div(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_div(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_div(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() / b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_div(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_div(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
        _ => None,
    }
}

fn exec_mod(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_rem(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_rem(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() % b.get()).map(Value::Float),
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
}

fn exec_neg(a: &Value) -> Option<Value> {
    match a {
        Value::Int(a) => a.checked_neg().map(Value::Int),
        Value::Float(a) => Float::new(-a.get()).map(Value::Float),
        Value::UInt(a) => i64::try_from(*a)
            .ok()
            .and_then(|v| v.checked_neg())
            .map(Value::Int),
        _ => None,
    }
}

fn exec_eq(a: &Value, b: &Value) -> Option<Value> {
    Some(Value::Bool(a == b))
}

fn exec_lt(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::Bool(a < b)),
        (Value::Int(a), Value::Int(b)) => Some(Value::Bool(a < b)),
        (Value::Float(a), Value::Float(b)) => Some(Value::Bool(a.get() < b.get())),
        (Value::UInt(a), Value::Int(b)) => Some(Value::Bool((*a as i128) < (*b as i128))),
        (Value::Int(a), Value::UInt(b)) => Some(Value::Bool((*a as i128) < (*b as i128))),
        (Value::UInt(a), Value::Float(b)) => Some(Value::Bool((*a as f64) < b.get())),
        (Value::Float(a), Value::UInt(b)) => Some(Value::Bool(a.get() < (*b as f64))),
        (Value::Int(a), Value::Float(b)) => Some(Value::Bool((*a as f64) < b.get())),
        (Value::Float(a), Value::Int(b)) => Some(Value::Bool(a.get() < (*b as f64))),
        _ => None,
    }
}

fn exec_not(a: &Value) -> Option<Value> {
    match a {
        Value::Bool(b) => Some(Value::Bool(!b)),
        _ => None,
    }
}

fn exec_bitand(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a & b)),
        _ => None,
    }
}

fn exec_bitor(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a | b)),
        _ => None,
    }
}

fn exec_bitxor(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a ^ b)),
        _ => None,
    }
}

fn exec_bitnot(a: &Value) -> Option<Value> {
    match a {
        Value::UInt(a) => Some(Value::UInt(!a)),
        _ => None,
    }
}

fn exec_shl(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a.wrapping_shl(*b as u32))),
        _ => None,
    }
}

fn exec_shr(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a.wrapping_shr(*b as u32))),
        _ => None,
    }
}

fn exec_bittest(x: &Value, b: &Value) -> Option<Value> {
    match (x, b) {
        (Value::UInt(x), Value::UInt(b)) => {
            if *b >= 64 {
                None
            } else {
                Some(Value::Bool((x >> b) & 1 == 1))
            }
        }
        _ => None,
    }
}

fn exec_bitset(x: &Value, b: &Value, v: &Value) -> Option<Value> {
    match (x, b, v) {
        (Value::UInt(x), Value::UInt(b), Value::Bool(v)) => {
            if *b >= 64 {
                None
            } else if *v {
                Some(Value::UInt(x | (1 << b)))
            } else {
                Some(Value::UInt(x & !(1 << b)))
            }
        }
        _ => None,
    }
}

fn exec_len(a: &Value) -> Option<Value> {
    match a {
        Value::Text(s) => Some(Value::UInt(s.chars().count() as u64)),
        Value::Bytes(b) => Some(Value::UInt(b.len() as u64)),
        Value::Array(arr) => Some(Value::UInt(arr.len() as u64)),
        Value::Map(map) => Some(Value::UInt(map.len() as u64)),
        Value::Sequence(seq) => seq.remaining().map(|n| Value::UInt(n as u64)),
        _ => None,
    }
}

fn exec_make_array(arg_slots: &[usize], vm: &mut VM) -> Result<Option<Value>, ExecError> {
    let elems: Vec<Value> = arg_slots
        .iter()
        .filter_map(|s| vm.local(*s).cloned())
        .collect();
    let arr = HeapVal::new(elems, vm.heap())?;
    Ok(Some(Value::Array(arr)))
}

fn exec_make_map(arg_slots: &[usize], vm: &mut VM) -> Result<Option<Value>, ExecError> {
    if !arg_slots.len().is_multiple_of(2) {
        return Ok(None);
    }
    let map: IndexMap<Value, Value> = arg_slots
        .chunks(2)
        .filter_map(|pair| {
            let k = vm.local(pair[0]).cloned()?;
            let v = vm.local(pair[1]).cloned()?;
            Some((k, v))
        })
        .collect();
    let heap_map = HeapVal::new(map, vm.heap())?;
    Ok(Some(Value::Map(heap_map)))
}

fn exec_make_seq(arg_slots: &[usize], vm: &mut VM) -> Option<Value> {
    let inclusive = match vm.local(arg_slots[2]) {
        Some(Value::Bool(b)) => *b,
        _ => false,
    };
    let seq = match (vm.local(arg_slots[0]), vm.local(arg_slots[1])) {
        (Some(Value::UInt(start)), Some(Value::UInt(end))) => Some(SeqState::RangeUInt {
            current: *start,
            end: *end,
            inclusive,
        }),
        (Some(Value::Int(start)), Some(Value::Int(end))) => Some(SeqState::RangeInt {
            current: *start,
            end: *end,
            inclusive,
        }),
        (Some(Value::UInt(start)), Some(Value::Int(end))) => Some(SeqState::RangeInt {
            current: *start as i64,
            end: *end,
            inclusive,
        }),
        (Some(Value::Int(start)), Some(Value::UInt(end))) => Some(SeqState::RangeInt {
            current: *start,
            end: *end as i64,
            inclusive,
        }),
        _ => None,
    };
    // HeapVal::new can fail, but for sequences this is infallible in practice.
    // Use try_into pattern to avoid changing the return type.
    seq.and_then(|state| HeapVal::new(state, vm.heap()).ok().map(Value::Sequence))
}

fn exec_array_seq(arg_slots: &[usize], vm: &mut VM) -> Option<Value> {
    let start = match vm.local(arg_slots[1]) {
        Some(Value::UInt(n)) => *n as usize,
        _ => return None,
    };
    let end = match vm.local(arg_slots[2]) {
        Some(Value::UInt(n)) => *n as usize,
        _ => return None,
    };
    let mutable = match vm.local(arg_slots[3]) {
        Some(Value::Bool(b)) => *b,
        _ => false,
    };
    match vm.local(arg_slots[0]) {
        Some(Value::Array(arr)) => {
            let state = SeqState::ArraySlice {
                source: arr.clone(),
                start,
                end,
                mutable,
            };
            HeapVal::new(state, vm.heap()).ok().map(Value::Sequence)
        }
        _ => None,
    }
}

fn exec_widen(arg_slots: &[usize], vm: &VM) -> Option<Value> {
    let target = match vm.local(arg_slots[1]) {
        Some(Value::UInt(t)) => *t,
        _ => return None,
    };
    let value = vm.local(arg_slots[0]);
    match (value, target) {
        (Some(Value::UInt(n)), 2) => {
            let n = *n;
            if n > i64::MAX as u64 {
                None
            } else {
                Some(Value::Int(n as i64))
            }
        }
        (Some(Value::Int(n)), 2) => Some(Value::Int(*n)),
        (Some(Value::UInt(n)), 3) => Float::new(*n as f64).map(Value::Float),
        (Some(Value::Int(n)), 3) => Float::new(*n as f64).map(Value::Float),
        (Some(Value::Float(f)), 3) => Some(Value::Float(*f)),
        _ => None,
    }
}

fn exec_cast(arg_slots: &[usize], vm: &VM) -> Option<Value> {
    let target = match vm.local(arg_slots[1]) {
        Some(Value::UInt(t)) => *t,
        _ => return None,
    };
    let value = vm.local(arg_slots[0]);
    match (value, target) {
        (Some(Value::UInt(n)), 1) => Some(Value::UInt(*n)),
        (Some(Value::Int(n)), 1) => Some(Value::UInt(*n as u64)),
        (Some(Value::UInt(n)), 2) => Some(Value::Int(*n as i64)),
        (Some(Value::Int(n)), 2) => Some(Value::Int(*n)),
        (Some(Value::UInt(n)), 3) => Float::new(*n as f64).map(Value::Float),
        (Some(Value::Int(n)), 3) => Float::new(*n as f64).map(Value::Float),
        (Some(Value::Float(f)), 3) => Some(Value::Float(*f)),
        _ => None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;

    /// Helper: compile source and execute a named function (no args)
    fn run(source: &str, func_name: &str) -> Result<Option<Value>, String> {
        let builtins = builtins::standard_builtins();
        let (program, diagnostics) =
            crate::compile(source, &builtins).map_err(|d| format!("compilation failed: {}", d))?;

        if diagnostics.has_warnings() {
            eprintln!("warnings: {}", diagnostics);
        }

        let mut vm = VM::new();
        program
            .call(&mut vm, func_name, 0)
            .map_err(|e| format!("exec error: {}", e))
    }

    /// Helper: compile and run, expecting a Value back
    fn run_expect(source: &str, func_name: &str) -> Value {
        run(source, func_name)
            .expect("should not error")
            .expect("should return a value")
    }

    // ========================================================================
    // Basic Execution
    // ========================================================================

    #[test]
    fn test_return_constant() {
        let val = run_expect("fn test() { return 42; }", "test");
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_return_bool() {
        let val = run_expect("fn test() { return true; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_return_no_value() {
        let result = run("fn test() { return; }", "test").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_implicit_return() {
        // Final expression without semicolon is the return value
        let val = run_expect("fn test() { 99 }", "test");
        assert_eq!(val, Value::UInt(99));
    }

    // ========================================================================
    // Arithmetic (binary builtins)
    // ========================================================================

    #[test]
    fn test_addition() {
        let val = run_expect("fn test() { return 1 + 2; }", "test");
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_arithmetic_expression() {
        let val = run_expect("fn test() { return (10 - 3) * 2; }", "test");
        assert_eq!(val, Value::UInt(14));
    }

    #[test]
    fn test_comparison() {
        let val = run_expect("fn test() { return 5 > 3; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_equality() {
        let val = run_expect("fn test() { return 42 == 42; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    // ========================================================================
    // Variables
    // ========================================================================

    #[test]
    fn test_let_binding() {
        let val = run_expect(
            "fn test() { let x = 10; let y = 20; return x + y; }",
            "test",
        );
        assert_eq!(val, Value::UInt(30));
    }

    #[test]
    fn test_variable_reassignment() {
        let val = run_expect("fn test() { let x = 1; x = x + 10; return x; }", "test");
        assert_eq!(val, Value::UInt(11));
    }

    // ========================================================================
    // Control Flow
    // ========================================================================

    #[test]
    fn test_if_true() {
        // Implicit return: if-expression is the final expression (no semicolon)
        let val = run_expect("fn test() { if true { 1 } else { 2 } }", "test");
        assert_eq!(val, Value::UInt(1));
    }

    #[test]
    fn test_if_false() {
        let val = run_expect("fn test() { if false { 1 } else { 2 } }", "test");
        assert_eq!(val, Value::UInt(2));
    }

    #[test]
    fn test_if_with_comparison() {
        let val = run_expect(
            "fn test() { let x = 10; if x > 5 { 1 } else { 0 } }",
            "test",
        );
        assert_eq!(val, Value::UInt(1));
    }

    // ========================================================================
    // Loops
    // ========================================================================

    #[test]
    fn test_while_loop() {
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                let i = 0;
                while i < 5 {
                    sum = sum + i;
                    i = i + 1;
                }
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(10)); // 0+1+2+3+4
    }

    #[test]
    fn test_loop_break() {
        let val = run_expect(
            r#"
            fn test() {
                let i = 0;
                loop {
                    if i >= 3 {
                        break;
                    }
                    i = i + 1;
                }
                return i;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_loop_break_with_value() {
        let val = run_expect(
            r#"
            fn test() {
                let result = loop {
                    break 42;
                };
                return result;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    // ========================================================================
    // Functions
    // ========================================================================

    #[test]
    fn test_function_call() {
        let val = run_expect(
            r#"
            fn add(a, b) { return a + b; }
            fn test() { return add(3, 4); }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(7));
    }

    #[test]
    fn test_recursive_function() {
        let val = run_expect(
            r#"
            fn factorial(n) {
                if n <= 1 { return 1; }
                return n * factorial(n - 1);
            }
            fn test() { return factorial(5); }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(120));
    }

    // ========================================================================
    // Constants
    // ========================================================================

    #[test]
    fn test_const_binding() {
        let val = run_expect(
            r#"
            const MAX = 100;
            fn test() { return MAX; }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(100));
    }

    // ========================================================================
    // Short-circuit logic
    // ========================================================================

    #[test]
    fn test_short_circuit_and() {
        let val = run_expect("fn test() { return true && false; }", "test");
        assert_eq!(val, Value::Bool(false));
    }

    #[test]
    fn test_short_circuit_or() {
        let val = run_expect("fn test() { return false || true; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    // ========================================================================
    // Builtins
    // ========================================================================

    #[test]
    fn test_len() {
        let val = run_expect(r#"fn test() { let a = [1, 2, 3]; return len(a); }"#, "test");
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_negation() {
        let val = run_expect("fn test() { return !true; }", "test");
        assert_eq!(val, Value::Bool(false));
    }

    // ========================================================================
    // Match / Pattern Matching
    // ========================================================================

    #[test]
    fn test_match_literal() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 2;
                match x {
                    1 => { return 10; },
                    2 => { return 20; },
                    3 => { return 30; },
                    _ => { return 0; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(20));
    }

    #[test]
    fn test_match_wildcard() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 99;
                match x {
                    1 => { return 10; },
                    _ => { return 42; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_match_type_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                match x {
                    Bool(b) => { return 0; },
                    UInt(n) => { return n; },
                    _ => { return 99; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_match_with_guard() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 15;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(1));
    }

    #[test]
    fn test_match_guard_fails() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 5;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(2));
    }

    // ========================================================================
    // If-Let / If-With Patterns
    // ========================================================================

    #[test]
    fn test_if_let_binding() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                if let y = x {
                    return y + 1;
                }
                return 0;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(43));
    }

    #[test]
    fn test_if_let_type_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                if let UInt(n) = x {
                    return n + 10;
                }
                return 0;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(52));
    }

    // ========================================================================
    // Array Destructuring
    // ========================================================================

    #[test]
    fn test_let_array_destructure() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let [a, b, c] = arr;
                return a + b + c;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_match_array_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2];
                match arr {
                    [a, b] => { return a + b; },
                    _ => { return 0; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(3));
    }

    // ========================================================================
    // For Loop Execution
    // ========================================================================

    #[test]
    fn test_for_array_sum() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_for_array_with_index() {
        // Pair binding: i = index, x = element
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let result = 0;
                for i, x in arr {
                    result = result + i + x;
                };
                return result;
            }
            "#,
            "test",
        );
        // (0+10) + (1+20) + (2+30) = 63
        assert_eq!(val, Value::UInt(63));
    }

    #[test]
    fn test_for_with_break() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x > 3 { break; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1 + 2 + 3 = 6 (stops before 4)
        assert_eq!(val, Value::UInt(6));
    }

    #[test]
    fn test_for_with_continue() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x == 3 { continue; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1 + 2 + 4 + 5 = 12 (skips 3)
        assert_eq!(val, Value::UInt(12));
    }

    #[test]
    fn test_for_empty_array() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [];
                let count = 0;
                for x in arr {
                    count = count + 1;
                };
                return count;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(0));
    }

    #[test]
    fn test_for_nested() {
        let val = run_expect(
            r#"
            fn test() {
                let a = [1, 2];
                let b = [10, 20];
                let sum = 0;
                for x in a {
                    for y in b {
                        sum = sum + x * y;
                    };
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1*10 + 1*20 + 2*10 + 2*20 = 10 + 20 + 20 + 40 = 90
        assert_eq!(val, Value::UInt(90));
    }

    #[test]
    fn test_for_let_binding() {
        // for let x — by-value, mutations don't affect source
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3];
                let sum = 0;
                for let x in arr {
                    x = x * 10;
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 10 + 20 + 30 = 60
        assert_eq!(val, Value::UInt(60));
    }

    // ========================================================================
    // Sequence / Range Execution
    // ========================================================================

    #[test]
    fn test_range_sum() {
        // for i in 0..5 { sum += i } → 0+1+2+3+4 = 10
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 0..5 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(10));
    }

    #[test]
    fn test_range_inclusive_sum() {
        // for i in 0..=4 { sum += i } → 0+1+2+3+4 = 10
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 0..=4 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(10));
    }

    #[test]
    fn test_range_empty() {
        // 5..3 is empty — body never runs
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 5..3 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(0));
    }

    #[test]
    fn test_range_with_break() {
        // 0..10 with break at 3 → 0+1+2 = 3
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 0..10 {
                    if i == 3 { break; };
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_range_with_continue() {
        // 0..6, skip even numbers → 1+3+5 = 9
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 0..6 {
                    if i % 2 == 0 { continue; };
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(9));
    }

    #[test]
    fn test_range_single_element() {
        // 5..6 has one element: 5
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                for i in 5..6 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(5));
    }

    #[test]
    fn test_range_nested() {
        // Nested ranges: for i in 0..3 { for j in 0..3 { count++ } }
        let val = run_expect(
            r#"
            fn test() {
                let count = 0;
                for i in 0..3 {
                    for j in 0..3 {
                        count = count + 1;
                    };
                };
                return count;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(9));
    }

    #[test]
    fn test_range_dynamic_bounds() {
        // Range with dynamic bounds from array length
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for i in 0..len(arr) {
                    sum = sum + arr[i];
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_range_as_value() {
        // Store a range in a variable, then iterate — type dispatch
        // selects the sequence path at runtime.
        let val = run_expect(
            r#"
            fn test() {
                let r = 1..4;
                let sum = 0;
                for i in r {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(6));
    }

    #[test]
    fn test_for_type_dispatch_array() {
        // Ensure index-based path still works through type dispatch
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_for_dispatch_with_accumulator() {
        // Outer variable modified in loop body — verify Phi merge at join
        let val = run_expect(
            r#"
            fn test() {
                let count = 0;
                for i in 0..5 {
                    count = count + 1;
                };
                return count;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(5));
    }

    // ========================================================================
    // collect() Intrinsic
    // ========================================================================

    #[test]
    fn test_collect_range() {
        // collect(0..5) → [0, 1, 2, 3, 4]
        let val = run_expect(
            r#"
            fn test() {
                let arr = collect(0..5);
                return len(arr);
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(5));
    }

    #[test]
    fn test_collect_range_sum() {
        // collect(0..4) then sum the array
        let val = run_expect(
            r#"
            fn test() {
                let arr = collect(1..=3);
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(6));
    }

    // ================================================================
    // Type cast (as) tests
    // ================================================================

    #[test]
    fn test_cast_uint_to_int() {
        let val = run_expect("fn test() { 42 as Int }", "test");
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_cast_int_to_uint_reinterpret() {
        // -1 as UInt should give u64::MAX (bit reinterpret)
        let val = run_expect("fn test() { -1 as UInt }", "test");
        assert_eq!(val, Value::UInt(u64::MAX));
    }

    #[test]
    fn test_cast_uint_to_int_reinterpret() {
        // Large UInt wraps to negative Int
        let val = run_expect(
            r#"
            fn test() {
                let x = 18446744073709551615 as Int;
                x
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::Int(-1));
    }

    #[test]
    fn test_cast_to_float() {
        let val = run_expect("fn test() { 42 as Float }", "test");
        assert_eq!(val, Value::Float(crate::exec::Float::new(42.0).unwrap()));
    }

    #[test]
    fn test_cast_int_to_float() {
        let val = run_expect("fn test() { -10 as Float }", "test");
        assert_eq!(val, Value::Float(crate::exec::Float::new(-10.0).unwrap()));
    }

    #[test]
    fn test_cast_identity() {
        // Same-type cast is identity
        let val = run_expect("fn test() { 42 as UInt }", "test");
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_cast_in_arithmetic() {
        // Cast then add
        let val = run_expect(
            r#"
            fn test() {
                let x = 10 as Float;
                let y = 3 as Float;
                x + y
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::Float(crate::exec::Float::new(13.0).unwrap()));
    }

    #[test]
    fn test_cast_chained() {
        // UInt → Int → UInt roundtrip
        let val = run_expect("fn test() { 42 as Int as UInt }", "test");
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_cast_precedence() {
        // x + y as Float should parse as x + (y as Float)
        // 10 + 5 as Float = 10 + 5.0
        // With implicit coercion, 10 (UInt) + 5.0 (Float) → 15.0
        let val = run_expect("fn test() { 10 + 5 as Float }", "test");
        assert_eq!(val, Value::Float(crate::exec::Float::new(15.0).unwrap()));
    }

    #[test]
    fn test_cast_const_fold() {
        // Constant cast should be folded at compile time
        let val = run_expect(
            r#"
            const X = -1 as UInt;
            fn test() { X }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(u64::MAX));
    }

    #[test]
    fn test_collect_empty_range() {
        // collect(5..3) → empty array
        let val = run_expect(
            r#"
            fn test() {
                let arr = collect(5..3);
                return len(arr);
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(0));
    }
}
