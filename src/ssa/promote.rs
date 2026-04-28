//! mem2reg: Promote Assign/Read instructions to SSA form.
//!
//! Implements Cytron et al. (1991) SSA construction using a dominator tree
//! (Cooper-Harvey-Kennedy 2001). Phi nodes are placed at iterated dominance
//! frontier blocks, then variables are renamed via a dominator-tree pre-order
//! walk.

use crate::ast;
use crate::ir::*;
use std::collections::{HashMap, HashSet, VecDeque};

use super::domtree::DominatorTree;

/// Promote all `Assign`/`Read` instructions in a function to SSA form.
///
/// After this pass:
/// - All `Assign` instructions are removed
/// - All `Read` instructions are replaced with `Copy` (or eliminated)
/// - Phi nodes are inserted at control flow merge points
/// - The function is in proper SSA form
pub fn promote(function: &mut Function) {
    let tree = DominatorTree::build(function);
    let mut ctx = PromoteCtx::new(function, &tree);
    ctx.place_phis(function, &tree);
    ctx.rename(function, &tree);
    ctx.eliminate_trivial_phis();

    ctx.apply(function);
}

// ============================================================================
// Promote context
// ============================================================================

/// Phi insertion map: block → [(dest_var, slot, [(pred_block, pred_var)])]
///
/// During phi placement, we insert placeholder phis with the slot they
/// correspond to. During renaming, the source operands are filled in.
struct PlacedPhi {
    dest: VarId,
    slot: u32,
    sources: Vec<(BlockId, VarId)>,
}

struct PromoteCtx {
    /// Placed phis per block (filled during place_phis, operands set during rename).
    placed_phis: HashMap<BlockId, Vec<PlacedPhi>>,

    /// Replacement map: for each `Read` instruction's dest VarId,
    /// the resolved SSA VarId to use instead.
    read_replacements: HashMap<VarId, VarId>,

    /// Substitution map for trivially eliminated phis.
    trivial_subst: HashMap<VarId, VarId>,

    /// Next VarId for fresh phi variables.
    next_var_id: u32,
}

impl PromoteCtx {
    fn new(function: &Function, _tree: &DominatorTree) -> Self {
        let max_var = function.locals.iter().map(|v| v.id.0).max().unwrap_or(0);

        PromoteCtx {
            placed_phis: HashMap::new(),
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

    // ── Phase A: Phi Placement ──────────────────────────────────────────

    /// Place phi nodes at iterated dominance frontier blocks for each slot.
    fn place_phis(&mut self, function: &Function, tree: &DominatorTree) {
        let df = tree.dominance_frontier();

        // Collect def sites: which blocks contain an Assign for each slot
        let mut def_sites: HashMap<u32, HashSet<BlockId>> = HashMap::new();
        // Also collect which slots exist at all
        let mut all_slots: HashSet<u32> = HashSet::new();

        for block in &function.blocks {
            for inst in &block.instructions {
                if let Instruction::Assign { slot, .. } = &inst.node {
                    def_sites.entry(*slot).or_default().insert(block.id);
                    all_slots.insert(*slot);
                }
            }
        }

        // For each slot, compute iterated dominance frontier and place phis
        for slot in all_slots {
            let sites = match def_sites.get(&slot) {
                Some(s) => s,
                None => continue,
            };

            // IDF computation: worklist algorithm
            let mut idf: HashSet<BlockId> = HashSet::new();
            let mut worklist: VecDeque<BlockId> = sites.iter().copied().collect();
            let mut processed: HashSet<BlockId> = HashSet::new();

            while let Some(block) = worklist.pop_front() {
                if let Some(frontier) = df.get(&block) {
                    for &df_block in frontier {
                        if idf.insert(df_block) && processed.insert(df_block) {
                            worklist.push_back(df_block);
                        }
                    }
                }
            }

            // Place a phi at each IDF block
            for idf_block in idf {
                let dest = self.fresh_var();
                let preds = tree
                    .predecessors()
                    .get(&idf_block)
                    .cloned()
                    .unwrap_or_default();
                let sources = preds.iter().map(|&p| (p, VarId(u32::MAX))).collect();
                self.placed_phis
                    .entry(idf_block)
                    .or_default()
                    .push(PlacedPhi {
                        dest,
                        slot,
                        sources,
                    });
            }
        }
    }

    // ── Phase B: Variable Renaming ──────────────────────────────────────

    /// Rename variables by walking the dominator tree in pre-order.
    fn rename(&mut self, function: &Function, tree: &DominatorTree) {
        let block_map: HashMap<BlockId, &BasicBlock> =
            function.blocks.iter().map(|b| (b.id, b)).collect();

        // Per-slot definition stack: top is the current reaching definition
        let mut stacks: HashMap<u32, Vec<VarId>> = HashMap::new();

        self.rename_block(tree.entry(), tree, &block_map, &mut stacks);
    }

    fn rename_block(
        &mut self,
        block: BlockId,
        tree: &DominatorTree,
        block_map: &HashMap<BlockId, &BasicBlock>,
        stacks: &mut HashMap<u32, Vec<VarId>>,
    ) {
        // Track how many definitions we push so we can pop them when leaving
        let mut push_count: HashMap<u32, usize> = HashMap::new();

        // 1. Process phis placed at this block — push their dests
        if let Some(phis) = self.placed_phis.get(&block) {
            for phi in phis {
                stacks.entry(phi.slot).or_default().push(phi.dest);
                *push_count.entry(phi.slot).or_default() += 1;
            }
        }

        // 2. Process instructions in this block
        if let Some(ir_block) = block_map.get(&block) {
            for inst in &ir_block.instructions {
                match &inst.node {
                    Instruction::Read { slot, dest } => {
                        let current = stacks.get(slot).and_then(|s| s.last()).copied();
                        match current {
                            Some(val) => {
                                self.read_replacements.insert(*dest, val);
                            }
                            None => {
                                // Read before any assignment — undefined
                                let undef = self.fresh_var();
                                self.read_replacements.insert(*dest, undef);
                            }
                        }
                    }
                    Instruction::Assign { slot, value } => {
                        stacks.entry(*slot).or_default().push(*value);
                        *push_count.entry(*slot).or_default() += 1;
                    }
                    _ => {}
                }
            }
        }

        // 3. Fill phi operands in successor blocks.
        //    Two sub-steps to satisfy the borrow checker: first collect
        //    slots that need values (immutable borrow of placed_phis),
        //    then resolve values (may need fresh_var for undefined), then
        //    write back (mutable borrow of placed_phis).
        if let Some(ir_block) = block_map.get(&block) {
            for succ in ir_block.terminator.successors() {
                // Collect the slots for phis in this successor
                let slots: Vec<u32> = self
                    .placed_phis
                    .get(&succ)
                    .map(|phis| phis.iter().map(|p| p.slot).collect())
                    .unwrap_or_default();

                if slots.is_empty() {
                    continue;
                }

                // Resolve values (may allocate fresh vars for undefined)
                let vals: Vec<VarId> = slots
                    .iter()
                    .map(|slot| {
                        stacks
                            .get(slot)
                            .and_then(|s| s.last())
                            .copied()
                            .unwrap_or_else(|| self.fresh_var())
                    })
                    .collect();

                // Write values into phi sources
                let phis = self.placed_phis.get_mut(&succ).unwrap();
                for (phi, val) in phis.iter_mut().zip(vals) {
                    for (pred, var) in &mut phi.sources {
                        if *pred == block {
                            *var = val;
                        }
                    }
                }
            }
        }

        // 4. Recurse into dominated children
        let children = tree.children(block);
        for child in children {
            self.rename_block(child, tree, block_map, stacks);
        }

        // 5. Pop definitions pushed in this block
        for (slot, count) in push_count {
            if let Some(stack) = stacks.get_mut(&slot) {
                for _ in 0..count {
                    stack.pop();
                }
            }
        }
    }

    // ── Phase C: Trivial Phi Elimination ────────────────────────────────

    fn eliminate_trivial_phis(&mut self) {
        // First pass: identify trivial phis
        for phis in self.placed_phis.values() {
            for phi in phis {
                let simplified = try_remove_trivial_phi(phi.dest, &phi.sources);
                if simplified != phi.dest {
                    self.trivial_subst.insert(phi.dest, simplified);
                }
            }
        }

        if self.trivial_subst.is_empty() {
            return;
        }

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

        // Apply substitutions to phi sources
        for phis in self.placed_phis.values_mut() {
            for phi in phis.iter_mut() {
                for (_, var) in &mut phi.sources {
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

        self.trivial_subst = resolved_subst;
    }

    // ── Apply ───────────────────────────────────────────────────────────

    /// Apply the computed SSA form back to the function.
    fn apply(self, function: &mut Function) {
        // 1. Insert Phi nodes at the start of blocks that need them.
        for block in &mut function.blocks {
            if let Some(phis) = self.placed_phis.get(&block.id) {
                let mut phi_instructions: Vec<SpannedInst> = Vec::new();
                for phi in phis {
                    // Skip trivially eliminated phis
                    if self.trivial_subst.contains_key(&phi.dest) {
                        continue;
                    }
                    phi_instructions.push(ast::Spanned::new(
                        Instruction::Phi {
                            dest: phi.dest,
                            sources: phi.sources.clone(),
                        },
                        ast::Span::default(),
                    ));
                }
                if !phi_instructions.is_empty() {
                    phi_instructions.append(&mut block.instructions);
                    block.instructions = phi_instructions;
                }
            }
        }

        // 2. Replace Read instructions with Copy, remove Assign instructions.
        for block in &mut function.blocks {
            block.instructions.retain_mut(|inst| match &inst.node {
                Instruction::Assign { .. } => false,
                Instruction::Read { dest, .. } => {
                    if let Some(&resolved) = self.read_replacements.get(dest) {
                        if resolved == *dest {
                            false
                        } else {
                            inst.node = Instruction::Copy {
                                dest: *dest,
                                src: resolved,
                            };
                            true
                        }
                    } else {
                        false
                    }
                }
                _ => true,
            });
        }

        // 3. Register new variables in the function's locals.
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
                TypeSet::any(),
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
            continue;
        }
        match same {
            None => same = Some(val),
            Some(s) if s == val => continue,
            Some(_) => return phi_var,
        }
    }
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
            blocks,
            locals,
            ..Default::default()
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
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::any()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::any()),
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
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::any()),
                Var::new(VarId(2), ast::Identifier("v2".into()), TypeSet::any()),
                Var::new(VarId(3), ast::Identifier("v3".into()), TypeSet::any()),
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
                Var::new(VarId(0), ast::Identifier("init".into()), TypeSet::any()),
                Var::new(VarId(1), ast::Identifier("r1".into()), TypeSet::any()),
                Var::new(VarId(2), ast::Identifier("body".into()), TypeSet::any()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::any()),
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
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::any()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::any()),
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
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::any()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::any()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::any()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Read of SLOT_OUTER should resolve to v0 (outer unaffected by inner).
        // Cytron may place a dead phi for SLOT_INNER at the merge — DCE removes it.
        let b3 = &func.blocks[3];
        let outer_phi = b3.instructions.iter().any(|i| match &i.node {
            Instruction::Phi { sources, .. } => {
                // A phi sourcing VarId(0) would mean it's for SLOT_OUTER
                sources.iter().any(|(_, v)| *v == VarId(0))
            }
            _ => false,
        });
        assert!(!outer_phi, "Outer slot should not have a phi");

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
