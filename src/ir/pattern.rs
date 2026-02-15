//! Pattern Lowering
//!
//! Handles pattern binding for let/with statements (unconditional binding).

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Pattern Lowering
    // ========================================================================

    /// Lower a pattern binding (for let/with statements)
    ///
    /// Emits diagnostics on error and continues processing.
    pub fn lower_pattern_binding(
        &mut self,
        pattern: &ast::Pattern,
        value: VarId,
        mode: BindingMode,
    ) {
        match pattern {
            ast::Pattern::Wildcard => {
                // Ignore the value
            }

            ast::Pattern::Variable(name) => {
                match mode {
                    BindingMode::Value => {
                        // Copy the value
                        let dest = self.new_var(name.clone(), TypeSet::from_types(all_types()));
                        self.emit(Instruction::Copy { dest, src: value });
                        self.bind(&name.0, dest);
                    }
                    BindingMode::Reference => {
                        // Just bind directly (mutations will go through SetIndex)
                        // TODO: Track reference info for mutations
                        self.bind(&name.0, value);
                    }
                }
            }

            ast::Pattern::Literal(_lit) => {
                // Literal patterns in let/with don't bind anything
                // They're for matching only (would need Guard terminator)
            }

            ast::Pattern::Array(patterns) => {
                // Destructure array
                for (i, pat) in patterns.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let elem = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });

                    self.lower_pattern_binding(&pat.node, elem, mode);
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                // TODO: Proper rest pattern handling
                // For now, just bind the before patterns
                for (i, pat) in before.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let elem = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: elem,
                        base: value,
                        key: idx,
                    });

                    self.lower_pattern_binding(&pat.node, elem, mode);
                }

                if let Some(rest_name) = rest {
                    // TODO: Slice the array for rest
                    let rest_var =
                        self.new_var(rest_name.clone(), TypeSet::single(types::BaseType::Array));
                    self.emit(Instruction::Undefined { dest: rest_var });
                    self.bind(&rest_name.0, rest_var);
                }

                let _ = after; // TODO: Handle after patterns
            }

            ast::Pattern::Map(entries) => {
                // Destructure map
                for (key_pat, val_pat) in entries {
                    // TODO: Key patterns need special handling
                    // For now, skip map destructuring
                    let _ = (key_pat, val_pat);
                }
            }

            ast::Pattern::Type { type_name, binding } => {
                // TODO: Type patterns need Match terminator
                // For now, just bind if there's a binding pattern
                if let Some(inner) = binding {
                    self.lower_pattern_binding(&inner.node, value, mode);
                }
                let _ = type_name;
            }
        }
    }
}
