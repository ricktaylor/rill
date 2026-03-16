//! Pattern Lowering
//!
//! Handles pattern binding for let/with statements (unconditional binding).

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Pattern Lowering
    // ========================================================================

    /// Lower a pattern binding (for let/with statements — unconditional)
    ///
    /// Unlike conditional patterns (if-let, match arms), unconditional bindings
    /// don't branch on mismatch. If a type/structure doesn't match, the bound
    /// variables are simply undefined (duck-typing: no error, undefined propagation).
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

            ast::Pattern::Variable(name) => match mode {
                BindingMode::Value => {
                    let dest = self.new_var(name.clone(), TypeSet::from_types(all_types()));
                    self.emit(Instruction::Copy { dest, src: value });
                    self.bind(name, dest);
                }
                BindingMode::Reference => {
                    self.bind(name, value);
                }
            },

            ast::Pattern::Literal(_lit) => {
                // Literal patterns in let/with don't bind anything.
                // They're only meaningful in conditional contexts (match, if-let).
            }

            ast::Pattern::Array(patterns) => {
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
                // Bind before elements (from start)
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

                // Compute length for rest and after patterns
                let length = self.emit_unary_call("len", value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // core::array_seq(array, start, end, mutable) -> Sequence(ArraySlice)
                //
                // Mutability follows the binding mode:
                //   let [a, ..rest] = arr   → mutable=false, iteration is by-value
                //   with [a, ..rest] = arr  → mutable=true, for-loop uses MakeRef
                //                             so mutations write back to arr
                if let Some(rest_name) = rest {
                    let start = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: start,
                        value: Literal::UInt(before.len() as u64),
                    });

                    let after_len = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let end = self.emit_binary_call("sub", length, after_len);

                    let is_mutable = self.new_temp(TypeSet::single(types::BaseType::Bool));
                    self.emit(Instruction::Const {
                        dest: is_mutable,
                        value: Literal::Bool(matches!(mode, BindingMode::Reference)),
                    });

                    let rest_val = self.new_temp(TypeSet::single(types::BaseType::Sequence));
                    self.emit(Instruction::Call {
                        dest: rest_val,
                        function: FunctionRef::core("array_seq"),
                        args: vec![
                            CallArg {
                                value,
                                by_ref: false,
                            },
                            CallArg {
                                value: start,
                                by_ref: false,
                            },
                            CallArg {
                                value: end,
                                by_ref: false,
                            },
                            CallArg {
                                value: is_mutable,
                                by_ref: false,
                            },
                        ],
                    });

                    let rest_var = self.new_var(
                        rest_name.clone(),
                        TypeSet::single(types::BaseType::Sequence),
                    );
                    self.emit(Instruction::Copy {
                        dest: rest_var,
                        src: rest_val,
                    });
                    self.bind(rest_name, rest_var);
                }

                // Bind after elements (from end, using len - after.len() + i)
                if !after.is_empty() {
                    let after_len = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let after_start = self.emit_binary_call("sub", length, after_len);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.new_temp(TypeSet::single(types::BaseType::UInt));
                        self.emit(Instruction::Const {
                            dest: offset,
                            value: Literal::UInt(i as u64),
                        });
                        let idx = self.emit_binary_call("add", after_start, offset);

                        let elem = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest: elem,
                            base: value,
                            key: idx,
                        });

                        self.lower_pattern_binding(&pat.node, elem, mode);
                    }
                }
            }

            ast::Pattern::Map(entries) => {
                // Destructure map: each entry has a key pattern (must be literal)
                // and a value pattern. Index into the map by key and bind the value.
                for (key_pat, val_pat) in entries {
                    // Key must be a literal for map destructuring
                    let key_var = match &key_pat.node {
                        ast::Pattern::Literal(lit) => self.lower_literal(lit),
                        ast::Pattern::Variable(name) => {
                            // Variable key: use the variable name as a text key
                            let key = self.new_temp(TypeSet::single(types::BaseType::Text));
                            self.emit(Instruction::Const {
                                dest: key,
                                value: Literal::Text(name.to_string()),
                            });
                            key
                        }
                        _ => {
                            self.diagnostics.error(
                                diagnostics::DiagnosticCode::E105_InvalidPattern,
                                self.current_span,
                                "map destructuring key must be a literal or identifier",
                            );
                            continue;
                        }
                    };

                    let val = self.new_temp(TypeSet::all());
                    self.emit(Instruction::Index {
                        dest: val,
                        base: value,
                        key: key_var,
                    });

                    self.lower_pattern_binding(&val_pat.node, val, mode);
                }
            }

            ast::Pattern::Type { type_name, binding } => {
                // Type pattern in unconditional binding: check type, bind if matches.
                // If type doesn't match, the binding is undefined (duck-typing).
                // We emit a Match terminator with a join block — if the type matches
                // we bind, otherwise variables get undefined.
                if let Some(base_type) = self.type_name_to_base_type(type_name) {
                    let match_bb = self.fresh_block();
                    let nomatch_bb = self.fresh_block();
                    let join_bb = self.fresh_block();

                    self.finish_block(Terminator::Match {
                        value,
                        arms: vec![(MatchPattern::Type(base_type), match_bb)],
                        default: nomatch_bb,
                        span: self.current_span,
                    });

                    // Match path: bind the inner pattern
                    self.current_block = match_bb;
                    self.current_instructions = Vec::new();
                    if let Some(inner) = binding {
                        self.lower_pattern_binding(&inner.node, value, mode);
                    }
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // No-match path: skip (variables remain unbound/undefined)
                    self.current_block = nomatch_bb;
                    self.current_instructions = Vec::new();
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Continue in join block
                    self.current_block = join_bb;
                    self.current_instructions = Vec::new();
                } else {
                    // Unknown type name — error already emitted by type_name_to_base_type
                }
            }
        }
    }
}
