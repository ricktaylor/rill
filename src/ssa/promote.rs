//! mem2reg: Promote Assign/Read instructions to SSA form.
//!
//! Implements the Braun et al. (2013) algorithm for on-demand SSA construction.

use crate::ast;
use crate::ir::*;
use std::collections::HashMap;

/// Promote all `Assign`/`Read` instructions in a function to SSA form.
///
/// After this pass:
/// - All `Assign` instructions are removed
/// - All `Read` instructions are replaced with `Copy` (or eliminated)
/// - Phi nodes are inserted at control flow merge points
/// - The function is in proper SSA form
pub fn promote(function: &mut Function) {
    let mut ctx = PromoteCtx::new(function);
    ctx.run_on_blocks(&function.blocks);

    ctx.apply(function);
}

// ============================================================================
// Predecessor map
// ============================================================================

/// Build a map from each block to its predecessor blocks.
///
/// Only includes reachable blocks (those reachable from the entry block via
/// forward traversal). Dead blocks — created after return/break/continue
/// statements — have no predecessors and would poison phi construction
/// with spurious undefined values if included.
fn build_predecessors(function: &Function) -> HashMap<BlockId, Vec<BlockId>> {
    // First: find all reachable blocks via BFS from entry
    let block_map: HashMap<BlockId, &BasicBlock> =
        function.blocks.iter().map(|b| (b.id, b)).collect();
    let mut reachable = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(function.entry_block);
    while let Some(bid) = queue.pop_front() {
        if !reachable.insert(bid) {
            continue;
        }
        if let Some(block) = block_map.get(&bid) {
            for succ in block.terminator.successors() {
                queue.push_back(succ);
            }
        }
    }

    // Build predecessor map using only reachable blocks
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for &bid in &reachable {
        preds.entry(bid).or_default();
    }
    for &bid in &reachable {
        if let Some(block) = block_map.get(&bid) {
            for succ in block.terminator.successors() {
                if reachable.contains(&succ) {
                    preds.entry(succ).or_default().push(bid);
                }
            }
        }
    }

    preds
}

// ============================================================================
// Promote context
// ============================================================================

struct PromoteCtx {
    /// Predecessor map: block → [predecessor blocks]
    predecessors: HashMap<BlockId, Vec<BlockId>>,

    /// Block-exit definitions from Pass 1 (immutable during Pass 2).
    /// Records the value of each slot at the END of each block (last Assign).
    /// Used for cross-block lookups during phi construction.
    block_exit_def: HashMap<(u32, BlockId), VarId>,

    /// Resolved definitions from Pass 2 (memoization).
    /// Caches the result of read_variable lookups so the same (slot, block)
    /// pair isn't resolved twice. Written during Pass 2, does NOT overwrite
    /// block_exit_def.
    resolved_def: HashMap<(u32, BlockId), VarId>,

    /// Phi nodes to insert at the start of blocks.
    /// `block_id → [(dest_var, [(pred_block, pred_var)])]`
    inserted_phis: HashMap<BlockId, Vec<(VarId, Vec<(BlockId, VarId)>)>>,

    /// Replacement map: for each `Read` instruction's dest VarId,
    /// the resolved SSA VarId to use instead.
    read_replacements: HashMap<VarId, VarId>,

    /// Substitution map for trivially eliminated phis.
    /// When a phi is trivial (all operands are the same value), its VarId
    /// is replaced by the simplified value. But the VarId may already be
    /// referenced in other phis' source lists. This map tracks those
    /// substitutions so they can be applied after all phis are constructed.
    trivial_subst: HashMap<VarId, VarId>,

    /// Next VarId for fresh phi variables.
    next_var_id: u32,
}

impl PromoteCtx {
    fn new(function: &Function) -> Self {
        // Find the highest existing VarId so new ones don't collide
        let max_var = function.locals.iter().map(|v| v.id.0).max().unwrap_or(0);

        PromoteCtx {
            predecessors: build_predecessors(function),
            block_exit_def: HashMap::new(),
            resolved_def: HashMap::new(),
            inserted_phis: HashMap::new(),
            read_replacements: HashMap::new(),
            trivial_subst: HashMap::new(),
            next_var_id: max_var + 1,
        }
    }

    fn fresh_var(&mut self) -> VarId {
        let id = VarId(self.next_var_id);
        self.next_var_id += 1;
        id
    }

    // ── Braun et al. core ────────────────────────────────────────────

    /// Look up the value for `slot` in `block`, checking block-exit
    /// definitions first, then resolved cache.
    ///
    /// block_exit_def takes precedence because it records the value AFTER
    /// all instructions in the block (the exit value). resolved_def records
    /// the entry value (phi placeholder or memoized lookup). Successors
    /// need the exit value — if the block assigns the slot, that assignment
    /// is what flows out, not the entry phi.
    fn lookup_def(&self, slot: u32, block: BlockId) -> Option<VarId> {
        self.block_exit_def
            .get(&(slot, block))
            .or_else(|| self.resolved_def.get(&(slot, block)))
            .copied()
    }

    /// Memoize a resolved value (Pass 2 only — does NOT touch block_exit_def).
    fn memoize(&mut self, slot: u32, block: BlockId, value: VarId) {
        self.resolved_def.insert((slot, block), value);
    }

    /// Look up the current SSA value for `slot` in `block`.
    ///
    /// If defined locally (resolved cache or block-exit), returns it directly.
    /// Otherwise, recursively queries predecessors, inserting Phi nodes at
    /// merge points.
    fn read_variable(&mut self, slot: u32, block: BlockId) -> VarId {
        if let Some(val) = self.lookup_def(slot, block) {
            return val;
        }
        self.read_variable_recursive(slot, block)
    }

    /// Predecessor lookup (the core of Braun et al.).
    ///
    /// Walks single-predecessor chains iteratively to avoid stack overflow
    /// on deep CFGs (common in if-else-if chains). Only recurses at merge
    /// points (multiple predecessors), where depth is bounded by the number
    /// of merge points.
    fn read_variable_recursive(&mut self, slot: u32, block: BlockId) -> VarId {
        // Walk single-predecessor chains iteratively
        let mut current = block;
        loop {
            let preds = self.predecessors.get(&current).cloned().unwrap_or_default();

            if preds.is_empty() {
                // Entry block — variable is undefined (read before assign).
                let undef = self.fresh_var();
                self.memoize(slot, current, undef);
                return self.memoize_chain(slot, block, current, undef);
            } else if preds.len() == 1 {
                // Single predecessor: check if it has a definition
                if let Some(val) = self.lookup_def(slot, preds[0]) {
                    self.memoize(slot, current, val);
                    return self.memoize_chain(slot, block, current, val);
                }
                // Keep walking (iterative, not recursive)
                current = preds[0];
            } else {
                // Multiple predecessors: insert a Phi node.
                //
                // Write a placeholder BEFORE recursing to break cycles.
                let phi_var = self.fresh_var();
                self.memoize(slot, current, phi_var);

                // Resolve each predecessor's value (recurses, but depth is
                // bounded by merge point count, not block count).
                let sources: Vec<(BlockId, VarId)> = preds
                    .iter()
                    .map(|&pred| (pred, self.read_variable(slot, pred)))
                    .collect();

                // Try to simplify trivial phis
                let simplified = try_remove_trivial_phi(phi_var, &sources);

                let val = if simplified != phi_var {
                    self.memoize(slot, current, simplified);
                    self.trivial_subst.insert(phi_var, simplified);
                    simplified
                } else {
                    self.inserted_phis
                        .entry(current)
                        .or_default()
                        .push((phi_var, sources));
                    phi_var
                };

                return self.memoize_chain(slot, block, current, val);
            }
        }
    }

    /// Memoize a value along the single-predecessor chain from `start` to `end`.
    fn memoize_chain(&mut self, slot: u32, start: BlockId, end: BlockId, val: VarId) -> VarId {
        if start != end {
            let mut current = start;
            let mut safety = self.predecessors.len();
            while current != end && safety > 0 {
                self.memoize(slot, current, val);
                let preds = self.predecessors.get(&current).cloned().unwrap_or_default();
                if preds.len() == 1 {
                    current = preds[0];
                } else {
                    break;
                }
                safety -= 1;
            }
        }
        val
    }

    // ── Main pass ────────────────────────────────────────────────────

    /// Resolve a Read by looking at predecessors, skipping the current block.
    ///
    /// Used when a Read appears before any Assign in the same block. We can't
    /// use `read_variable` because `current_def` has the block-exit value
    /// (from Pass 1), which would incorrectly return a later Assign's value.
    fn read_from_predecessors(&mut self, slot: u32, block: BlockId) -> VarId {
        let preds = self.predecessors.get(&block).cloned().unwrap_or_default();

        if preds.is_empty() {
            self.fresh_var()
        } else if preds.len() == 1 {
            self.read_variable(slot, preds[0])
        } else {
            // Multiple predecessors: insert phi.
            // Write placeholder to resolved_def (not block_exit_def) to break
            // cycles without corrupting Pass 1 data.
            let phi_var = self.fresh_var();
            self.memoize(slot, block, phi_var);

            let sources: Vec<(BlockId, VarId)> = preds
                .iter()
                .map(|&pred| (pred, self.read_variable(slot, pred)))
                .collect();

            let simplified = try_remove_trivial_phi(phi_var, &sources);
            if simplified != phi_var {
                self.memoize(slot, block, simplified);
                self.trivial_subst.insert(phi_var, simplified);
                simplified
            } else {
                self.inserted_phis
                    .entry(block)
                    .or_default()
                    .push((phi_var, sources));
                phi_var
            }
        }
    }

    /// Process all blocks from the function, resolving Assign/Read.
    ///
    /// Two-phase approach:
    /// - **Pass 1**: Record the block-exit value for each slot (last Assign per
    ///   block). This is stored in `current_def` and is immutable during Pass 2.
    ///   It ensures back-edge predecessors have definitions available for phi
    ///   construction.
    /// - **Pass 2**: Process instructions in order within each block. Uses a
    ///   per-block `local_def` to track intra-block state. Reads before any
    ///   same-block Assign use `read_from_predecessors` (skips the block-exit
    ///   value). Reads after a same-block Assign use the local value.
    ///
    /// This separation ensures:
    /// - Back-edges work: the body's Assign is in `current_def`, so the header
    ///   phi picks up the correct post-body value.
    /// - Intra-block ordering works: a Read before an Assign sees the incoming
    ///   value, not the later assigned value.
    fn run_on_blocks(&mut self, blocks: &[BasicBlock]) {
        // Pass 1: record block-exit definitions (last Assign per slot per block)
        // into block_exit_def. This map is immutable during Pass 2.
        for block in blocks {
            for spanned_inst in &block.instructions {
                if let Instruction::Assign { slot, value } = &spanned_inst.node {
                    self.block_exit_def.insert((*slot, block.id), *value);
                }
            }
        }

        // Pass 2: resolve Reads using per-block local state.
        for block in blocks {
            // local_def tracks Assigns seen so far in THIS block (instruction order).
            let mut local_def: HashMap<u32, VarId> = HashMap::new();

            for spanned_inst in &block.instructions {
                match &spanned_inst.node {
                    Instruction::Assign { slot, value } => {
                        local_def.insert(*slot, *value);
                    }
                    Instruction::Read { slot, dest } => {
                        let resolved = if let Some(&val) = local_def.get(slot) {
                            // A same-block Assign came before this Read — use it.
                            val
                        } else {
                            // No same-block Assign yet — get the incoming value
                            // from predecessors (NOT the block-exit value).
                            self.read_from_predecessors(*slot, block.id)
                        };
                        self.read_replacements.insert(*dest, resolved);
                    }
                    _ => {}
                }
            }
        }

        // Pass 3: Apply trivial phi substitutions.
        //
        // When a phi is trivially eliminated (all operands are the same value),
        // its VarId may already be referenced in other phis' source lists
        // (because those phis were constructed before the trivial phi was
        // simplified). Replace eliminated VarIds with their simplified values.
        if !self.trivial_subst.is_empty() {
            // Resolve chains: if a → b and b → c, then a → c
            let resolved_subst: HashMap<VarId, VarId> = self
                .trivial_subst
                .keys()
                .map(|&k| {
                    let mut v = k;
                    for _ in 0..64 {
                        match self.trivial_subst.get(&v) {
                            Some(&next) if next != v => v = next,
                            _ => break,
                        }
                    }
                    (k, v)
                })
                .collect();

            for phis in self.inserted_phis.values_mut() {
                for (_, sources) in phis {
                    for (_, var) in sources.iter_mut() {
                        if let Some(&replacement) = resolved_subst.get(var) {
                            *var = replacement;
                        }
                    }
                }
            }

            // Also fix read_replacements that reference eliminated phis
            for val in self.read_replacements.values_mut() {
                if let Some(&replacement) = resolved_subst.get(val) {
                    *val = replacement;
                }
            }
        }
    }

    /// Apply the computed SSA form back to the function.
    fn apply(self, function: &mut Function) {
        // 1. Insert Phi nodes at the start of blocks that need them.
        for block in &mut function.blocks {
            if let Some(phis) = self.inserted_phis.get(&block.id) {
                let mut phi_instructions: Vec<SpannedInst> = phis
                    .iter()
                    .map(|(dest, sources)| {
                        ast::Spanned::new(
                            Instruction::Phi {
                                dest: *dest,
                                sources: sources.clone(),
                            },
                            ast::Span::default(),
                        )
                    })
                    .collect();
                // Prepend phis before existing instructions
                phi_instructions.append(&mut block.instructions);
                block.instructions = phi_instructions;
            }
        }

        // 2. Replace Read instructions with Copy, remove Assign instructions.
        for block in &mut function.blocks {
            block.instructions.retain_mut(|inst| match &inst.node {
                Instruction::Assign { .. } => false, // Remove
                Instruction::Read { dest, .. } => {
                    // Replace with Copy to the resolved value
                    if let Some(&resolved) = self.read_replacements.get(dest) {
                        if resolved == *dest {
                            // Self-copy: remove entirely
                            false
                        } else {
                            inst.node = Instruction::Copy {
                                dest: *dest,
                                src: resolved,
                            };
                            true
                        }
                    } else {
                        // No resolution found — shouldn't happen in well-formed IR
                        false
                    }
                }
                _ => true, // Keep all other instructions
            });
        }

        // 3. Register new variables in the function's locals.
        //
        // The compiler maps VarId(n) → stack slot (1 + n), so ALL VarIds up to
        // next_var_id must have entries in locals to avoid out-of-bounds slot
        // access. fresh_var() may create VarIds for trivially eliminated phis
        // that aren't in inserted_phis — we must fill those gaps too.
        let existing_max = function
            .locals
            .iter()
            .map(|v| v.id.0)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        for id in existing_max..self.next_var_id {
            function.locals.push(Var::new(
                VarId(id),
                ast::Identifier("_phi".to_string()),
                TypeSet::all(),
            ));
        }
    }
}

// ============================================================================
// Trivial phi elimination
// ============================================================================

/// Check if a phi is trivial: all operands are the same value (ignoring
/// self-references to the phi itself).
///
/// Returns `phi_var` if non-trivial, or the single reaching value if trivial.
fn try_remove_trivial_phi(phi_var: VarId, sources: &[(BlockId, VarId)]) -> VarId {
    let mut same: Option<VarId> = None;
    for &(_, val) in sources {
        if val == phi_var {
            continue; // Self-reference: skip
        }
        match same {
            None => same = Some(val),
            Some(s) if s == val => continue, // Same as existing: skip
            Some(_) => return phi_var,       // Different values: non-trivial
        }
    }
    // If all operands were self-references, the phi is unreachable/undefined.
    // Otherwise, return the single value.
    same.unwrap_or(phi_var)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(id: u32, instructions: Vec<Instruction>, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            instructions: instructions
                .into_iter()
                .map(|inst| ast::Spanned::new(inst, ast::Span::default()))
                .collect(),
            terminator,
        }
    }

    fn make_function(blocks: Vec<BasicBlock>, locals: Vec<Var>) -> Function {
        Function {
            name: ast::Identifier("test".to_string()),
            params: Vec::new(),
            rest_param: None,
            locals,
            blocks,
            entry_block: BlockId(0),
        }
    }

    const SLOT_X: u32 = 0;

    /// Straight-line code: Assign then Read in the same block.
    /// Read should resolve to the assigned value — no phi needed.
    #[test]
    fn test_single_block_assign_read() {
        let mut func = make_function(
            vec![make_block(
                0,
                vec![
                    Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(0),
                    },
                    Instruction::Read {
                        slot: SLOT_X,
                        dest: VarId(1),
                    },
                ],
                Terminator::Return {
                    value: Some(VarId(1)),
                },
            )],
            vec![
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::all()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::all()),
            ],
        );

        promote(&mut func);

        // The Read should be replaced with Copy { dest: v1, src: v0 }
        // The Assign should be removed
        assert_eq!(func.blocks[0].instructions.len(), 1);
        match &func.blocks[0].instructions[0].node {
            Instruction::Copy { dest, src } => {
                assert_eq!(*dest, VarId(1));
                assert_eq!(*src, VarId(0));
            }
            other => panic!("Expected Copy, got {other:?}"),
        }
    }

    /// Diamond CFG: assign different values in then/else branches,
    /// read after the merge. Should insert a Phi.
    #[test]
    fn test_diamond_phi() {
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![],
                    Terminator::If {
                        condition: VarId(0),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    1,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(1),
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(
                    2,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(2),
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(
                    3,
                    vec![Instruction::Read {
                        slot: SLOT_X,
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("cond".into()), TypeSet::bool()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::all()),
                Var::new(VarId(2), ast::Identifier("v2".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("v3".into()), TypeSet::all()),
            ],
        );

        promote(&mut func);

        // Block 3 should now start with a Phi, followed by a Copy
        let b3 = &func.blocks[3];
        assert!(!b3.instructions.is_empty());

        // First instruction should be a Phi
        match &b3.instructions[0].node {
            Instruction::Phi { sources, .. } => {
                assert_eq!(sources.len(), 2);
                let block_ids: Vec<BlockId> = sources.iter().map(|(b, _)| *b).collect();
                assert!(block_ids.contains(&BlockId(1)));
                assert!(block_ids.contains(&BlockId(2)));
            }
            other => panic!("Expected Phi, got {other:?}"),
        }
    }

    /// Loop: x is assigned before the loop, modified in the body,
    /// read after. Should insert a loop-carried Phi in the header.
    #[test]
    fn test_loop_phi() {
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(0),
                    }],
                    Terminator::Jump { target: BlockId(1) },
                ),
                make_block(
                    1,
                    vec![Instruction::Read {
                        slot: SLOT_X,
                        dest: VarId(1),
                    }],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(2),
                        else_target: BlockId(3),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    2,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(2),
                    }],
                    Terminator::Jump { target: BlockId(1) },
                ),
                make_block(
                    3,
                    vec![Instruction::Read {
                        slot: SLOT_X,
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("init".into()), TypeSet::all()),
                Var::new(VarId(1), ast::Identifier("r1".into()), TypeSet::all()),
                Var::new(VarId(2), ast::Identifier("body".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::all()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Block 1 (header) should have a Phi with sources from block 0 and block 2
        let b1 = &func.blocks[1];
        let phi = b1
            .instructions
            .iter()
            .find(|i| matches!(i.node, Instruction::Phi { .. }));
        assert!(phi.is_some(), "Header block should have a Phi node");

        match &phi.unwrap().node {
            Instruction::Phi { sources, .. } => {
                let block_ids: Vec<BlockId> = sources.iter().map(|(b, _)| *b).collect();
                assert!(
                    block_ids.contains(&BlockId(0)),
                    "Phi should have entry edge"
                );
                assert!(block_ids.contains(&BlockId(2)), "Phi should have back-edge");
            }
            _ => unreachable!(),
        }
    }

    /// Trivial phi: both branches assign the same value.
    /// The phi should be eliminated.
    #[test]
    fn test_trivial_phi_elimination() {
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(0),
                    }],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    1,
                    vec![Instruction::Assign {
                        slot: SLOT_X,
                        value: VarId(0), // same value
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(2, vec![], Terminator::Jump { target: BlockId(3) }),
                make_block(
                    3,
                    vec![Instruction::Read {
                        slot: SLOT_X,
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::all()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Block 3 should have NO phi (trivial elimination)
        let b3 = &func.blocks[3];
        let has_phi = b3
            .instructions
            .iter()
            .any(|i| matches!(i.node, Instruction::Phi { .. }));
        assert!(!has_phi, "Trivial phi should be eliminated");

        // The Read should resolve directly to v0
        let copy = b3
            .instructions
            .iter()
            .find(|i| matches!(i.node, Instruction::Copy { .. }));
        match copy.map(|c| &c.node) {
            Some(Instruction::Copy { dest, src }) => {
                assert_eq!(*dest, VarId(3));
                assert_eq!(*src, VarId(0));
            }
            _ => panic!("Expected Copy to v0"),
        }
    }

    /// Shadowing: different slots for inner and outer variables with the same name.
    /// Inner slot should not affect outer slot after scope exit.
    #[test]
    fn test_shadowing_different_slots() {
        const SLOT_OUTER: u32 = 0;
        const SLOT_INNER: u32 = 1;

        // Block 0: outer_x = v0, branch
        // Block 1: inner_x = v1, jump to 3
        // Block 2: jump to 3
        // Block 3: read outer_x → should be v0 (not affected by inner)
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![Instruction::Assign {
                        slot: SLOT_OUTER,
                        value: VarId(0),
                    }],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    1,
                    vec![Instruction::Assign {
                        slot: SLOT_INNER,
                        value: VarId(1),
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(2, vec![], Terminator::Jump { target: BlockId(3) }),
                make_block(
                    3,
                    vec![Instruction::Read {
                        slot: SLOT_OUTER,
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::all()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::all()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Read of SLOT_OUTER should resolve to v0 — no phi needed
        let b3 = &func.blocks[3];
        let has_phi = b3
            .instructions
            .iter()
            .any(|i| matches!(i.node, Instruction::Phi { .. }));
        assert!(!has_phi, "Outer slot should not be affected by inner slot");

        let copy = b3
            .instructions
            .iter()
            .find(|i| matches!(i.node, Instruction::Copy { .. }));
        match copy.map(|c| &c.node) {
            Some(Instruction::Copy { dest, src }) => {
                assert_eq!(*dest, VarId(3));
                assert_eq!(*src, VarId(0));
            }
            _ => panic!("Expected Copy to v0"),
        }
    }
}
