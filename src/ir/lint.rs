//! Lowering-time lints computed on the pre-SSA function.

use super::*;
use std::collections::HashSet;

impl<'a> Lowerer<'a> {
    /// Emit `W001_UnusedVariable` for body value bindings (`let`/`for`/pattern/
    /// match) that are never read.
    ///
    /// Runs on the **pre-SSA** function (before `promote`), where `Read{slot}`
    /// instructions and the slot↔name map (`slot_decls`) are still available —
    /// user variable names do not survive SSA construction. Parameters
    /// (slot < `body_slot_start`), `with`/ref bindings (they go through
    /// `bind_ref`, never `new_slot`), and the discard `_` are excluded by
    /// construction.
    pub(super) fn check_unused_bindings(&mut self, function: &Function) {
        // Slots read anywhere in reachable code. A read only in dead code does
        // not keep a binding alive (it matches what codegen/DCE drop).
        let block_map = cfg::block_map(function);
        let reachable = cfg::reachable_blocks(function, &block_map);
        let mut read_slots: HashSet<u32> = HashSet::new();
        for block in &function.blocks {
            if !reachable.contains(&block.id) {
                continue;
            }
            for inst in &block.instructions {
                if let Instruction::Read { slot, .. } = &inst.node {
                    read_slots.insert(*slot);
                }
            }
        }

        // Body bindings, in slot order for deterministic diagnostics.
        let mut slots: Vec<u32> = self.slot_decls.keys().copied().collect();
        slots.sort_unstable();
        for slot in slots {
            if slot < self.body_slot_start || read_slots.contains(&slot) {
                continue;
            }
            let (name, span) = self.slot_decls[&slot].clone();
            self.diagnostics
                .warning(
                    diagnostics::DiagnosticCode::W001_UnusedVariable,
                    span,
                    format!("unused variable `{name}`"),
                )
                .help(format!(
                    "if this is intentional, prefix it with an underscore: `_{name}`"
                ));
        }
    }
}
