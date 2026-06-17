//! SSA Construction via Cytron et al. (1991) with Cooper-Harvey-Kennedy
//! dominator tree.
//!
//! Converts pre-SSA IR (containing `Assign` and `Read` instructions) into
//! proper SSA form with Phi nodes at control flow merge points.
//!
//! # Algorithm
//!
//! Two-phase Cytron approach backed by a dominator tree:
//!
//! 1. **Phi placement:** Compute the iterated dominance frontier (IDF) for
//!    each variable's definition sites. Insert placeholder Phi nodes at
//!    each IDF block.
//!
//! 2. **Variable renaming:** Walk the dominator tree in pre-order, maintaining
//!    a definition stack per variable. Reads resolve to the top of the stack;
//!    assignments and phis push new definitions.
//!
//! The dominator tree is computed using the Cooper-Harvey-Kennedy (2001)
//! iterative algorithm and is available for reuse by optimizer passes
//! (LICM, GVN, bounds checking).
//!
//! Trivial Phi nodes (all operands are the same value, or self-referential)
//! are eliminated after renaming. Remaining redundancies are handled
//! by the existing copy propagation and DCE optimizer passes.
//!
//! # Usage
//!
//! After the lowerer emits `Assign`/`Read` instructions, call
//! `promote(&mut function)` to convert to SSA form. This replaces
//! all `Assign`/`Read` instructions with proper SSA VarIds and Phi nodes.

pub(crate) mod domtree;
pub(crate) mod liveness;
pub(crate) mod slot_alloc;
mod promote;

pub use promote::promote;
