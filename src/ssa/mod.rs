//! SSA Construction via mem2reg (Braun et al. 2013)
//!
//! Converts pre-SSA IR (containing `Assign` and `Read` instructions) into
//! proper SSA form with Phi nodes at control flow merge points.
//!
//! # Algorithm
//!
//! Implements "Simple and Efficient Construction of Static Single Assignment
//! Form" (Braun, Buchwald, Hack, Leißa, Mallon, Zwinkau — CC 2013).
//!
//! The algorithm constructs SSA form on-the-fly by recursively querying
//! predecessor blocks for variable definitions:
//!
//! - **Single predecessor:** inherit the value directly
//! - **Multiple predecessors:** insert a Phi node, fill operands recursively
//! - **No predecessors (entry block):** variable is a parameter or undefined
//!
//! Trivial Phi nodes (all operands are the same value, or self-referential)
//! are eliminated during construction. Remaining redundancies are handled
//! by the existing copy propagation and DCE optimizer passes.
//!
//! # Usage
//!
//! After the lowerer emits `Assign`/`Read` instructions, call
//! `mem2reg::promote(&mut function)` to convert to SSA form. This replaces
//! all `Assign`/`Read` instructions with proper SSA VarIds and Phi nodes.

mod promote;
