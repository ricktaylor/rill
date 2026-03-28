//! IR-to-Closure Compilation and Execution
//!
//! Compiles the SSA-form IR into closure-threaded code for execution.
//! Each IR instruction becomes a Rust closure that captures its operands
//! (slot offsets, resolved extern function pointers, constant values).
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
//! - Externs resolved once at compile time (no runtime HashMap lookup)
//! - VarIds mapped to stack slot offsets at compile time
//! - Loops use iterative block dispatch (no Rust stack growth)
//! - User function calls use recursive inline loops (bounded by VM stack limit)
//! - Future: tail-call optimization can convert recursive calls to jumps

mod exec;
mod specialize;
mod terminator;

#[cfg(test)]
mod tests;

use crate::diagnostics::Diagnostics;
use crate::exec::{ExecError, Float, HeapVal, SeqState, VM, Value};
use crate::externs::{ExecResult, ExternImpl, ExternRegistry};
use crate::ir::opt::TypeAnalysis;
use crate::ir::{
    BasicBlock, BlockId, Function, Instruction, IntrinsicOp, IrProgram, Literal, MatchPattern,
    Terminator, VarId,
};
use crate::types::{BaseType, TypeSet};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

// Re-export submodule items used internally by compile_instruction
use exec::*;
use specialize::*;
use terminator::*;

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
pub(crate) struct CompiledFunction {
    /// All step closures for all blocks, flattened into a single contiguous array.
    /// Block boundaries are recorded in `block_starts`.
    pub(crate) steps: Vec<Step>,
    /// Offset into `steps` where each block begins.
    /// `block_starts[i]` is the index of the first step in block i.
    pub(crate) block_starts: Vec<usize>,
    pub(crate) entry: usize, // index into block_starts
    pub(crate) frame_size: usize,
    pub(crate) param_count: usize,
}

/// A step closure. Captures operands, operates on VM.
/// Instructions return Continue; terminators return NextBlock/Return/Exit.
pub(crate) type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

/// Result of executing a step.
pub(crate) enum Action {
    /// Continue to the next step in this block
    Continue,
    /// Jump to another block
    NextBlock(usize),
    /// Return from function
    Return(Option<Value>),
    /// Hard exit to driver (diverging extern)
    Exit(Value),
}

// ============================================================================
// Slot Mapping
// ============================================================================

/// Maps VarIds to stack slot offsets.
/// Slot 0 = Frame info. VarId(n) → slot n + 1.
pub(crate) fn slot(var: VarId) -> usize {
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
    externs: &ExternRegistry,
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
        externs,
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
pub(crate) enum CallTarget {
    /// Native extern — function pointer resolved at compile time.
    /// Includes optional type-specialized variants for monomorphic dispatch.
    Extern {
        generic: crate::externs::ExternFn,
        /// Variants: (param TypeSets, return TypeSet, specialized fn pointer)
        variants: Vec<(Vec<TypeSet>, TypeSet, crate::externs::ExternFn)>,
    },
    /// User-defined function — index into CompiledProgram.functions
    UserFunction(usize),
}

/// Map from qualified function name to its resolved target.
pub(crate) type LinkMap = HashMap<String, CallTarget>;

/// Link phase: resolve all function references and track usage.
fn link_functions(
    ir: &IrProgram,
    externs: &ExternRegistry,
    func_index: &HashMap<String, usize>,
    used_functions: &mut HashSet<usize>,
    diagnostics: &mut Diagnostics,
) -> LinkMap {
    let mut link_map = LinkMap::new();

    // Pre-populate with all externs
    for (name, def) in externs.iter() {
        if let ExternImpl::Native(f) = &def.implementation {
            let variants: Vec<(Vec<TypeSet>, TypeSet, crate::externs::ExternFn)> = def
                .variants
                .iter()
                .filter_map(|v| {
                    if let ExternImpl::Native(vf) = &v.implementation {
                        Some((v.param_types.clone(), v.returns, *vf))
                    } else {
                        None
                    }
                })
                .collect();
            link_map.insert(
                name.clone(),
                CallTarget::Extern {
                    generic: *f,
                    variants,
                },
            );
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
pub(crate) struct RefMeta {
    pub(crate) base_var: VarId,
    pub(crate) key_slot: Option<usize>,
}

impl RefMeta {
    pub(crate) fn base_slot(&self) -> usize {
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
                Some(CallTarget::Extern { generic, variants }) => {
                    // Try to select a type-specialized variant at compile time
                    let f = if !variants.is_empty() {
                        let arg_types: Vec<TypeSet> = args
                            .iter()
                            .map(|a| {
                                types
                                    .get_at_exit(block_id, a.value)
                                    .copied()
                                    .unwrap_or(TypeSet::all())
                            })
                            .collect();
                        variants
                            .iter()
                            .find(|(param_types, _, _)| {
                                param_types.len() == arg_types.len()
                                    && param_types.iter().zip(&arg_types).all(|(spec, actual)| {
                                        !actual.is_empty() && actual.difference(spec).is_empty()
                                    })
                            })
                            .map(|(_, _, vf)| *vf)
                            .unwrap_or(generic)
                    } else {
                        generic
                    };

                    let argc = arg_slots.len();
                    Box::new(move |vm: &mut VM, _prog| {
                        // Set up frame for extern (same convention as user functions)
                        let caller_bp = vm.bp();
                        let frame_size = 1 + argc; // slot 0 = Frame, slots 1..=N = args
                        vm.call(frame_size, None)?;

                        // Copy args from caller's slots into extern's frame
                        for (i, (s, _by_ref)) in arg_slots.iter().enumerate() {
                            if let Some(val) = vm.get(caller_bp + s).cloned() {
                                vm.set_local(i + 1, val);
                            }
                        }

                        // Call extern — reads args via vm.arg(i)
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
