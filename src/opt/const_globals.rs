//! Const-global inlining
//!
//! Recovers the zero-cost behaviour of the removed `const` keyword. A file-scope
//! `let NAME = <foldable>;` that no function ever writes is effectively a
//! constant — it is initialized once in `__init__` and only ever read. This pass
//! replaces every `LoadGlobal` of such a global with the constant itself, drops
//! the now-dead `StoreGlobal` from `__init__`, and compacts the remaining global
//! slots. A program whose only globals are constants ends up with `global_count
//! == 0` and no `__init__` — exactly what `const` used to compile to.
//!
//! Globals that a function writes (mutable module state) land in `external_writes`
//! and are left as runtime globals, and globals with non-foldable initializers
//! (`let t = now();`) keep their `__init__` store — they run at load time with the
//! actually-linked externs, which is the whole reason `const` was removed.

use crate::diagnostics::Diagnostics;
use crate::externs::ExternRegistry;
use crate::ir::{Instruction, IrProgram, Literal, VarId};
use std::collections::{HashMap, HashSet};

const INIT_NAME: &str = "__init__";

/// Inline never-written foldable globals to constants. Returns `true` if the IR
/// changed (the caller should re-run per-function optimization to fold the
/// inlined constants into surrounding code).
pub fn inline_const_globals(
    program: &mut IrProgram,
    externs: &ExternRegistry,
    diagnostics: &mut Diagnostics,
) -> bool {
    if program.global_count == 0 {
        return false;
    }
    let Some(init_idx) = program
        .functions
        .iter()
        .position(|f| f.name.as_ref() == INIT_NAME)
    else {
        return false;
    };

    let mut changed = false;

    // Fixpoint: inlining one global can make a dependent global foldable
    // (`let DOUBLE = MAX_TTL * 2;` folds once `MAX_TTL` is a constant).
    loop {
        // Re-optimize __init__ so foldable initializers collapse to single Consts.
        super::optimize_function(&mut program.functions[init_idx], externs, diagnostics);

        // Slots written by some function other than __init__ are mutable globals.
        let mut external_writes: HashSet<u32> = HashSet::new();
        for (i, func) in program.functions.iter().enumerate() {
            if i == init_idx {
                continue;
            }
            for block in &func.blocks {
                for inst in &block.instructions {
                    if let Instruction::StoreGlobal { slot, .. } = &inst.node {
                        external_writes.insert(*slot);
                    }
                }
            }
        }

        // From __init__: literal-valued Const defs, per-slot store count/value, and
        // the slots that __init__ itself reads. A global read during init may be a
        // forward reference (`let b = a + 1; let a = 10;` → b reads a as Undefined),
        // so inlining it would wrongly fold the constant into an earlier
        // initializer. Such globals are left as runtime globals.
        let init = &program.functions[init_idx];
        let mut const_defs: HashMap<VarId, Literal> = HashMap::new();
        let mut store_count: HashMap<u32, usize> = HashMap::new();
        let mut store_value: HashMap<u32, VarId> = HashMap::new();
        let mut init_reads: HashSet<u32> = HashSet::new();
        for block in &init.blocks {
            for inst in &block.instructions {
                match &inst.node {
                    Instruction::Const { dest, value } => {
                        const_defs.insert(*dest, value.clone());
                    }
                    Instruction::StoreGlobal { slot, value } => {
                        *store_count.entry(*slot).or_insert(0) += 1;
                        store_value.insert(*slot, *value);
                    }
                    Instruction::LoadGlobal { slot, .. } => {
                        init_reads.insert(*slot);
                    }
                    _ => {}
                }
            }
        }

        // A slot is inlinable (not externally written, not read during init) when
        // it is either never stored (always Undefined) or stored exactly once with
        // a Const value.
        let mut inlinable: HashMap<u32, Literal> = HashMap::new();
        for slot in 0..program.global_count as u32 {
            if external_writes.contains(&slot) || init_reads.contains(&slot) {
                continue;
            }
            match store_count.get(&slot).copied().unwrap_or(0) {
                0 => {
                    inlinable.insert(slot, Literal::Undefined);
                }
                1 => {
                    if let Some(lit) = const_defs.get(&store_value[&slot]) {
                        inlinable.insert(slot, lit.clone());
                    }
                }
                _ => {} // multiple stores in __init__ — leave as a runtime global
            }
        }
        if inlinable.is_empty() {
            break;
        }

        // Replace LoadGlobal of inlinable slots with the constant, and drop their
        // StoreGlobal (which only ever appears in __init__). A round that touches
        // no instruction (e.g. an already-inlined, unreferenced slot) ends the loop.
        let mut changed_this_round = false;
        for func in &mut program.functions {
            for block in &mut func.blocks {
                let before = block.instructions.len();
                block.instructions.retain(|inst| {
                    !matches!(&inst.node, Instruction::StoreGlobal { slot, .. }
                        if inlinable.contains_key(slot))
                });
                if block.instructions.len() != before {
                    changed_this_round = true;
                }
                for inst in &mut block.instructions {
                    if let Instruction::LoadGlobal { dest, slot } = &inst.node
                        && let Some(lit) = inlinable.get(slot)
                    {
                        inst.node = Instruction::Const {
                            dest: *dest,
                            value: lit.clone(),
                        };
                        changed_this_round = true;
                    }
                }
            }
        }
        if !changed_this_round {
            break;
        }
        changed = true;
    }

    // Compaction: renumber the slots still referenced to 0..M, dropping any that
    // were inlined away (or were never used at all).
    let mut live: HashSet<u32> = HashSet::new();
    for func in &program.functions {
        for block in &func.blocks {
            for inst in &block.instructions {
                if let Instruction::LoadGlobal { slot, .. } | Instruction::StoreGlobal { slot, .. } =
                    &inst.node
                {
                    live.insert(*slot);
                }
            }
        }
    }
    let mut live_sorted: Vec<u32> = live.into_iter().collect();
    live_sorted.sort_unstable();
    let new_count = live_sorted.len();
    if new_count != program.global_count {
        let remap: HashMap<u32, u32> = live_sorted
            .iter()
            .enumerate()
            .map(|(i, &s)| (s, i as u32))
            .collect();
        for func in &mut program.functions {
            for block in &mut func.blocks {
                for inst in &mut block.instructions {
                    if let Instruction::LoadGlobal { slot, .. }
                    | Instruction::StoreGlobal { slot, .. } = &mut inst.node
                    {
                        *slot = remap[slot];
                    }
                }
            }
        }
        program.global_count = new_count;
        changed = true;
    }

    // No globals left → drop the now-empty __init__ entirely (zero overhead).
    if program.global_count == 0 {
        program.functions.retain(|f| f.name.as_ref() != INIT_NAME);
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the full parse → lower → optimize pipeline and return the optimized IR.
    fn optimize_source(source: &str) -> IrProgram {
        let externs = crate::externs::standard_externs();
        let mut diags = Diagnostics::new();
        let ast = crate::ast::parser::parse(source, "<test>", &mut diags).expect("parse");
        let mut ir = crate::ir::lower(&ast, &externs, &mut diags).expect("lower");
        super::super::optimize(&mut ir, &externs, &mut diags);
        ir
    }

    fn has_load_global(ir: &IrProgram) -> bool {
        ir.functions.iter().any(|f| {
            f.blocks.iter().any(|b| {
                b.instructions
                    .iter()
                    .any(|i| matches!(i.node, Instruction::LoadGlobal { .. }))
            })
        })
    }

    fn has_init(ir: &IrProgram) -> bool {
        ir.functions.iter().any(|f| f.name.as_ref() == INIT_NAME)
    }

    #[test]
    fn const_only_program_fully_inlined() {
        // A never-written foldable global compiles to zero globals and no __init__.
        let ir = optimize_source("let MAX = 100; fn f() { ::MAX }");
        assert_eq!(ir.global_count, 0, "const-only global should be inlined away");
        assert!(!has_load_global(&ir), "LoadGlobal should become Const");
        assert!(!has_init(&ir), "__init__ dropped when no globals remain");
    }

    #[test]
    fn global_read_during_init_not_inlined() {
        // `A` is read by `B`'s initializer, so it can't be inlined (a later global
        // might forward-reference it as Undefined). Both stay runtime globals.
        let ir = optimize_source("let A = 86400; let B = A * 2; fn f() { ::B }");
        assert_eq!(ir.global_count, 2);
        assert!(has_init(&ir));
    }

    #[test]
    fn mutable_global_not_inlined() {
        // A global written by a function stays a runtime global.
        let ir = optimize_source(
            "let count = 0; fn inc() { ::count = ::count + 1; } fn get() { ::count }",
        );
        assert_eq!(ir.global_count, 1, "mutable global must be kept");
        assert!(has_load_global(&ir), "mutable global read via LoadGlobal");
        assert!(has_init(&ir), "__init__ retained for the mutable global");
    }
}
