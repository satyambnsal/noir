//! The loop invariant code motion pass moves code from inside a loop to before the loop
//! if that code will always have the same result on every iteration of the loop.
//!
//! To identify a loop invariant, check whether all of an instruction's values are:
//! - Outside of the loop
//! - Constant
//! - Already marked as loop invariants
//!
//! We also check that we are not hoisting instructions with side effects.
//! However, there are certain instructions whose side effects are only activated
//! under a predicate (e.g. an array out of bounds error on a dynamic index).
//! Thus, we also track the control dependence of loop blocks to determine
//! whether these "pure with predicate instructions" can be hoisted.
//! We use post-dominance frontiers to determine control dependence.
//!
//! Let's look at definition 3 from the following paper:
//! Jeanne Ferrante, Karl J. Ottenstein, and Joe D. Warren. 1987.
//! The program dependence graph and its use in optimization. ACM
//! Trans. Program. Lang. Syst. 9, 3 (July 1987), 319–349.
//! <https://doi.org/10.1145/24039.24041>
//!
//! ```text
//! Let G be a control flow graph. Let X and Y be nodes in G. Y is
//! control dependent on X iff
//! (1) there exists a directed path P from X to Y with any 2 in P (excluding X
//! and Y) post-dominated by Y and
//! (2) X is not post-dominated by Y.
//! ```
//!
//! Verifying these conditions for every loop block would be quite inefficient.
//! For example, let's say we just want to check whether a given loop block is control dependent at all
//! after the loop preheader. We would have to to verify the conditions above for every block between the loop preheader
//! and the given loop block. This is n^2 complexity in the worst case.
//! To optimize the control dependence checks, we can use post-dominance frontiers (PDF).
//!
//! From Cooper, Keith D. et al. “A Simple, Fast Dominance Algorithm.” (1999).
//! ```text
//! A dominance frontier is the set of all CFG nodes, y, such that
//! b dominates a predecessor of y but does not strictly dominate y.
//! ```
//! Reversing this for post-dominance we can see that the conditions for control dependence
//! are the same as those for post-dominance frontiers.
//! Thus, we rewrite our control dependence condition as Y is control dependent on X iff Y is in PDF(Y).
//!
//! We then can store the PDFs for every block as part of the context of this pass, and use it for checking control dependence.
//! Using PDFs gets us from a worst case n^2 complexity to a worst case n.
use acvm::{FieldElement, acir::AcirField};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::ssa::{
    Ssa,
    ir::{
        basic_block::BasicBlockId,
        cfg::ControlFlowGraph,
        dom::DominatorTree,
        function::Function,
        function_inserter::FunctionInserter,
        instruction::{
            Binary, BinaryOp, Instruction, InstructionId, binary::eval_constant_binary_op,
        },
        post_order::PostOrder,
        types::Type,
        value::ValueId,
    },
};

use super::unrolling::{Loop, Loops};

impl Ssa {
    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) fn loop_invariant_code_motion(mut self) -> Ssa {
        for function in self.functions.values_mut() {
            function.loop_invariant_code_motion();
        }

        self
    }
}

impl Function {
    pub(super) fn loop_invariant_code_motion(&mut self) {
        Loops::find_all(self).hoist_loop_invariants(self);
    }
}

impl Loops {
    fn hoist_loop_invariants(mut self, function: &mut Function) {
        let mut context = LoopInvariantContext::new(function);

        // The loops should be sorted by the number of blocks.
        // We want to access outer nested loops first, which we do by popping
        // from the top of the list.
        while let Some(loop_) = self.yet_to_unroll.pop() {
            let Ok(pre_header) = loop_.get_pre_header(context.inserter.function, &self.cfg) else {
                // If the loop does not have a preheader we skip hoisting loop invariants for this loop
                continue;
            };

            context.current_pre_header = Some(pre_header);
            context.hoist_loop_invariants(&loop_);
        }

        context.map_dependent_instructions();
        context.inserter.map_data_bus_in_place();
    }
}

impl Loop {
    /// Find the value that controls whether to perform a loop iteration.
    /// This is going to be the block parameter of the loop header.
    ///
    /// Consider the following example of a `for i in 0..4` loop:
    /// ```text
    /// brillig(inline) fn main f0 {
    ///   b0(v0: u32):
    ///     ...
    ///     jmp b1(u32 0)
    ///   b1(v1: u32):                  // Loop header
    ///     v5 = lt v1, u32 4           // Upper bound
    ///     jmpif v5 then: b3, else: b2
    /// ```
    /// In the example above, `v1` is the induction variable
    fn get_induction_variable(&self, function: &Function) -> ValueId {
        function.dfg.block_parameters(self.header)[0]
    }
}

struct LoopInvariantContext<'f> {
    inserter: FunctionInserter<'f>,
    defined_in_loop: HashSet<ValueId>,
    loop_invariants: HashSet<ValueId>,
    // Maps current loop induction variable -> fixed lower and upper loop bound
    // This map is expected to only ever contain a singular value.
    // However, we store it in a map in order to match the definition of
    // `outer_induction_variables` as both maps share checks for evaluating binary operations.
    current_induction_variables: HashMap<ValueId, (FieldElement, FieldElement)>,
    // Maps outer loop induction variable -> fixed lower and upper loop bound
    // This will be used by inner loops to determine whether they
    // have safe operations reliant upon an outer loop's maximum induction variable.
    outer_induction_variables: HashMap<ValueId, (FieldElement, FieldElement)>,
    // This context struct processes runs across all loops.
    // This stores the current loop's pre-header block.
    // It is wrapped in an Option as our SSA `Id<T>` does not allow dummy values.
    current_pre_header: Option<BasicBlockId>,

    cfg: ControlFlowGraph,

    // Stores whether the current block being processed is control dependent
    current_block_control_dependent: bool,

    // Maps a block to its post-dominance frontiers
    // This map should be precomputed a single time and used for checking control dependence.
    post_dom_frontiers: HashMap<BasicBlockId, HashSet<BasicBlockId>>,
}

impl<'f> LoopInvariantContext<'f> {
    fn new(function: &'f mut Function) -> Self {
        let cfg = ControlFlowGraph::with_function(function);
        let reversed_cfg = cfg.reverse();
        let post_order = PostOrder::with_cfg(&reversed_cfg);
        let mut post_dom = DominatorTree::with_cfg_and_post_order(&reversed_cfg, &post_order);
        let post_dom_frontiers = post_dom.compute_dominance_frontiers(&reversed_cfg);
        Self {
            inserter: FunctionInserter::new(function),
            defined_in_loop: HashSet::default(),
            loop_invariants: HashSet::default(),
            current_induction_variables: HashMap::default(),
            outer_induction_variables: HashMap::default(),
            current_pre_header: None,
            cfg,
            current_block_control_dependent: false,
            post_dom_frontiers,
        }
    }

    fn pre_header(&self) -> BasicBlockId {
        self.current_pre_header.expect("ICE: Pre-header block should have been set")
    }

    fn hoist_loop_invariants(&mut self, loop_: &Loop) {
        self.set_values_defined_in_loop(loop_);

        for block in loop_.blocks.iter() {
            self.is_control_dependent_post_pre_header(loop_, *block);

            for instruction_id in self.inserter.function.dfg[*block].take_instructions() {
                self.transform_to_unchecked_from_loop_bounds(instruction_id);

                let hoist_invariant = self.can_hoist_invariant(instruction_id);

                if hoist_invariant {
                    self.inserter.push_instruction(instruction_id, self.pre_header());

                    // If we are hoisting a MakeArray instruction,
                    // we need to issue an extra inc_rc in case they are mutated afterward.
                    if self.inserter.function.runtime().is_brillig()
                        && matches!(
                            self.inserter.function.dfg[instruction_id],
                            Instruction::MakeArray { .. }
                        )
                    {
                        let result =
                            self.inserter.function.dfg.instruction_results(instruction_id)[0];
                        let inc_rc = Instruction::IncrementRc { value: result };
                        let call_stack = self
                            .inserter
                            .function
                            .dfg
                            .get_instruction_call_stack_id(instruction_id);
                        self.inserter
                            .function
                            .dfg
                            .insert_instruction_and_results(inc_rc, *block, None, call_stack);
                    }
                } else {
                    self.inserter.push_instruction(instruction_id, *block);
                }
                self.extend_values_defined_in_loop_and_invariants(instruction_id, hoist_invariant);
            }
        }

        self.set_induction_var_bounds(loop_, false);
    }

    /// Checks whether a `block` is control dependent on any blocks after
    /// the given loop's header.
    fn is_control_dependent_post_pre_header(&mut self, loop_: &Loop, block: BasicBlockId) {
        let all_predecessors = Loop::find_blocks_in_loop(loop_.header, block, &self.cfg).blocks;

        // Need to accurately determine whether the current block is dependent on any blocks between
        // the current block and the loop header, exclusive of the current block and loop header themselves
        if all_predecessors
            .into_iter()
            .filter(|&predecessor| predecessor != block && predecessor != loop_.header)
            .any(|predecessor| self.is_control_dependent(predecessor, block))
        {
            self.current_block_control_dependent = true;
        }
    }

    /// Checks whether a `block` is control dependent on a `parent_block`
    /// Uses post-dominance frontiers to determine control dependence.
    /// Reference the doc comments at the top of the this module for more information
    /// regarding post-dominance frontiers and control dependence.
    fn is_control_dependent(&mut self, parent_block: BasicBlockId, block: BasicBlockId) -> bool {
        match self.post_dom_frontiers.get(&block) {
            Some(dependent_blocks) => dependent_blocks.contains(&parent_block),
            None => false,
        }
    }

    /// Gather the variables declared within the loop
    fn set_values_defined_in_loop(&mut self, loop_: &Loop) {
        // Clear any values that may be defined in previous loops, as the context is per function.
        self.defined_in_loop.clear();
        // These are safe to keep per function, but we want to be clear that these values
        // are used per loop.
        self.loop_invariants.clear();
        // There is only ever one current induction variable for a loop.
        // For a new loop, we clear the previous induction variable and then
        // set the new current induction variable.
        self.current_induction_variables.clear();
        self.set_induction_var_bounds(loop_, true);
        // The previous loop may have set that the current block is control dependent.
        // If we fail to reset for the next loop, a block may be inadvertently labelled
        // as control dependent thus preventing optimizations.
        self.current_block_control_dependent = false;

        for block in loop_.blocks.iter() {
            let params = self.inserter.function.dfg.block_parameters(*block);
            self.defined_in_loop.extend(params);
            for instruction_id in self.inserter.function.dfg[*block].instructions() {
                let results = self.inserter.function.dfg.instruction_results(*instruction_id);
                self.defined_in_loop.extend(results);
            }
        }
    }

    /// Update any values defined in the loop and loop invariants after a
    /// analyzing and re-inserting a loop's instruction.
    fn extend_values_defined_in_loop_and_invariants(
        &mut self,
        instruction_id: InstructionId,
        hoist_invariant: bool,
    ) {
        let results = self.inserter.function.dfg.instruction_results(instruction_id).to_vec();
        // We will have new IDs after pushing instructions.
        // We should mark the resolved result IDs as also being defined within the loop.
        let results =
            results.into_iter().map(|value| self.inserter.resolve(value)).collect::<Vec<_>>();
        self.defined_in_loop.extend(results.iter());

        // We also want the update result IDs when we are marking loop invariants as we may not
        // be going through the blocks of the loop in execution order
        if hoist_invariant {
            // Track already found loop invariants
            self.loop_invariants.extend(results.iter());
        }
    }

    fn can_hoist_invariant(&mut self, instruction_id: InstructionId) -> bool {
        use Instruction::*;

        let mut is_loop_invariant = true;
        // The list of blocks for a nested loop contain any inner loops as well.
        // We may have already re-inserted new instructions if two loops share blocks
        // so we need to map all the values in the instruction which we want to check.
        let (instruction, _) = self.inserter.map_instruction(instruction_id);
        instruction.for_each_value(|value| {
            // If an instruction value is defined in the loop and not already a loop invariant
            // the instruction results are not loop invariants.
            //
            // We are implicitly checking whether the values are constant as well.
            // The set of values defined in the loop only contains instruction results and block parameters
            // which cannot be constants.
            is_loop_invariant &=
                !self.defined_in_loop.contains(&value) || self.loop_invariants.contains(&value);
        });

        let can_be_hoisted = instruction.can_be_hoisted(self.inserter.function, false)
            || matches!(instruction, MakeArray { .. })
            || (instruction.can_be_hoisted(self.inserter.function, true)
                && !self.current_block_control_dependent)
            || self.can_be_hoisted_from_loop_bounds(&instruction);

        is_loop_invariant && can_be_hoisted
    }

    /// Keep track of a loop induction variable and respective upper bound.
    /// In the case of a nested loop, this will be used by later loops to determine
    /// whether they have operations reliant upon the maximum induction variable.
    /// When within the current loop, the known upper bound can be used to simplify instructions,
    /// such as transforming a checked add to an unchecked add.
    fn set_induction_var_bounds(&mut self, loop_: &Loop, current_loop: bool) {
        let bounds = loop_.get_const_bounds(self.inserter.function, self.pre_header());
        if let Some((lower_bound, upper_bound)) = bounds {
            let induction_variable = loop_.get_induction_variable(self.inserter.function);
            let induction_variable = self.inserter.resolve(induction_variable);
            if current_loop {
                self.current_induction_variables
                    .insert(induction_variable, (lower_bound, upper_bound));
            } else {
                self.outer_induction_variables
                    .insert(induction_variable, (lower_bound, upper_bound));
            }
        }
    }

    /// Certain instructions can take advantage of that our induction variable has a fixed minimum/maximum.
    ///
    /// For example, an array access can usually only be safely deduplicated when we have a constant
    /// index that is below the length of the array.
    /// Checking an array get where the index is the loop's induction variable on its own
    /// would determine that the instruction is not safe for hoisting.
    /// However, if we know that the induction variable's upper bound will always be in bounds of the array
    /// we can safely hoist the array access.
    fn can_be_hoisted_from_loop_bounds(&self, instruction: &Instruction) -> bool {
        use Instruction::*;

        match instruction {
            ArrayGet { array, index } => {
                let array_typ = self.inserter.function.dfg.type_of_value(*array);
                let upper_bound = self.outer_induction_variables.get(index).map(|bounds| bounds.1);
                if let (Type::Array(_, len), Some(upper_bound)) = (array_typ, upper_bound) {
                    upper_bound.to_u128() <= len.into()
                } else {
                    false
                }
            }
            Binary(binary) => self.can_evaluate_binary_op(binary, &self.outer_induction_variables),
            Constrain(..) | ConstrainNotEqual(..) | RangeCheck { .. } => {
                // These instructions should not be hoisted if we know the loop will never be executed (an upper bound or zero or equal loop bounds)
                // or we are unsure if the loop will ever be executed (dynamic loop bounds).
                // If the instruction were to be hoisted out of a loop that never executes it could potentially cause the program to fail when it is not meant to fail.
                let bounds = self.current_induction_variables.values().next().copied();
                let does_loop_body_execute = bounds
                    .map(|(lower_bound, upper_bound)| !(upper_bound - lower_bound).is_zero())
                    .unwrap_or(false);
                // If we know the loop will be executed these instructions can still only be hoisted if the instructions
                // are in a non control dependent block.
                does_loop_body_execute && !self.current_block_control_dependent
            }
            _ => false,
        }
    }

    /// Binary operations can take advantage of that our induction variable has a fixed minimum/maximum,
    /// to be transformed from a checked operation to an unchecked operation.
    ///
    /// Checked operations require more bytecode and thus we aim to minimize their usage wherever possible.
    ///
    /// For example, if one side of an add/mul operation is a constant and the other is an induction variable
    /// with a known upper bound, we know whether that binary operation will ever overflow.
    /// If we determine that an overflow is not possible we can convert the checked operation to unchecked.
    fn transform_to_unchecked_from_loop_bounds(&mut self, instruction_id: InstructionId) {
        let Instruction::Binary(binary) = &self.inserter.function.dfg[instruction_id] else {
            return;
        };

        if binary.operator.is_unchecked()
            || !self.can_evaluate_binary_op(binary, &self.current_induction_variables)
        {
            return;
        }

        if let Instruction::Binary(binary) = &mut self.inserter.function.dfg[instruction_id] {
            binary.operator = binary.operator.into_unchecked();
        };
    }

    /// Checks whether a binary operation can be evaluated using the bounds of a given loop induction variables.
    ///
    /// If it cannot be evaluated, it means that we either have a dynamic loop bound or
    /// that the operation can potentially overflow during a given loop iteration.
    fn can_evaluate_binary_op(
        &self,
        binary: &Binary,
        induction_vars: &HashMap<ValueId, (FieldElement, FieldElement)>,
    ) -> bool {
        let operand_type = self.inserter.function.dfg.type_of_value(binary.lhs).unwrap_numeric();

        let lhs_const = self.inserter.function.dfg.get_numeric_constant_with_type(binary.lhs);
        let rhs_const = self.inserter.function.dfg.get_numeric_constant_with_type(binary.rhs);
        let (lhs, rhs) = match (
            lhs_const,
            rhs_const,
            induction_vars.get(&binary.lhs),
            induction_vars.get(&binary.rhs),
        ) {
            (Some((lhs, _)), None, None, Some((lower_bound, upper_bound))) => {
                if matches!(binary.operator, BinaryOp::Div | BinaryOp::Mod) {
                    // If we have a Div/Mod operation we want to make sure that the
                    // lower bound is not zero.
                    (lhs, *lower_bound)
                } else {
                    (lhs, *upper_bound)
                }
            }
            (None, Some((rhs, _)), Some((lower_bound, upper_bound)), None) => {
                if matches!(binary.operator, BinaryOp::Sub { .. }) {
                    // If we are subtracting and the induction variable is on the lhs,
                    // we want to check the induction variable lower bound.
                    (*lower_bound, rhs)
                } else {
                    (*upper_bound, rhs)
                }
            }
            _ => return false,
        };

        // We evaluate this expression using the upper bounds (or lower in the case of div/mod)
        // of its inputs to check whether it will ever overflow.
        // If so, this will cause `eval_constant_binary_op` to return `None`.
        // Therefore a `Some` value shows that this operation is safe.
        eval_constant_binary_op(lhs, rhs, binary.operator, operand_type).is_some()
    }

    /// Loop invariant hoisting only operates over loop instructions.
    /// The `FunctionInserter` is used for mapping old values to new values after
    /// re-inserting loop invariant instructions.
    /// However, there may be instructions which are not within loops that are
    /// still reliant upon the instruction results altered during the pass.
    /// This method re-inserts all instructions so that all instructions have
    /// correct new value IDs based upon the `FunctionInserter` internal map.
    /// Leaving out this mapping could lead to instructions with values that do not exist.
    fn map_dependent_instructions(&mut self) {
        let mut block_order = PostOrder::with_function(self.inserter.function).into_vec();
        block_order.reverse();

        for block in block_order {
            for instruction_id in self.inserter.function.dfg[block].take_instructions() {
                self.inserter.push_instruction(instruction_id, block);
            }
            self.inserter.map_terminator_in_place(block);
        }
    }
}

#[cfg(test)]
mod test {
    use crate::ssa::Ssa;
    use crate::ssa::opt::assert_normalized_ssa_equals;

    #[test]
    fn simple_loop_invariant_code_motion() {
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
              jmp b1(i32 0)
          b1(v2: i32):
              v5 = lt v2, i32 4
              jmpif v5 then: b3, else: b2
          b2():
              return
          b3():
              v6 = mul v0, v1
              constrain v6 == i32 6
              v8 = unchecked_add v2, i32 1
              jmp b1(v8)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let main = ssa.main();

        let instructions = main.dfg[main.entry_block()].instructions();
        assert_eq!(instructions.len(), 0); // The final return is not counted

        // From b3:
        // ```
        // v6 = mul v0, v1
        // constrain v6 == i32 6
        // ```
        // To b0:
        // ```
        // v3 = mul v0, v1
        // constrain v3 == i32 6
        // ```
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            v3 = mul v0, v1
            constrain v3 == i32 6
            jmp b1(i32 0)
          b1(v2: i32):
            v7 = lt v2, i32 4
            jmpif v7 then: b3, else: b2
          b2():
            return
          b3():
            v9 = unchecked_add v2, i32 1
            jmp b1(v9)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn nested_loop_invariant_code_motion() {
        // Check that a loop invariant in the inner loop of a nested loop
        // is hoisted to the parent loop's pre-header block.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            jmp b1(i32 0)
          b1(v2: i32):
            v6 = lt v2, i32 4
            jmpif v6 then: b3, else: b2
          b2():
            return
          b3():
            jmp b4(i32 0)
          b4(v3: i32):
            v7 = lt v3, i32 4
            jmpif v7 then: b6, else: b5
          b5():
            v9 = unchecked_add v2, i32 1
            jmp b1(v9)
          b6():
            v10 = mul v0, v1
            constrain v10 == i32 6
            v12 = unchecked_add v3, i32 1
            jmp b4(v12)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let main = ssa.main();

        let instructions = main.dfg[main.entry_block()].instructions();
        assert_eq!(instructions.len(), 0); // The final return is not counted

        // `v10 = mul v0, v1` in b6 should now be `v4 = mul v0, v1` in b0
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            v4 = mul v0, v1
            constrain v4 == i32 6
            jmp b1(i32 0)
          b1(v2: i32):
            v8 = lt v2, i32 4
            jmpif v8 then: b3, else: b2
          b2():
            return
          b3():
            jmp b4(i32 0)
          b4(v3: i32):
            v9 = lt v3, i32 4
            jmpif v9 then: b6, else: b5
          b5():
            v12 = unchecked_add v2, i32 1
            jmp b1(v12)
          b6():
            v11 = unchecked_add v3, i32 1
            jmp b4(v11)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn hoist_invariant_with_invariant_as_argument() {
        // Check that an instruction which has arguments defined in the loop
        // but which are already marked loop invariants is still hoisted to the preheader.
        //
        // For example, in b3 we have the following instructions:
        // ```text
        // v6 = mul v0, v1
        // v7 = mul v6, v0
        // ```
        // `v6` should be marked a loop invariants as `v0` and `v1` are both declared outside of the loop.
        // As we will be hoisting `v6 = mul v0, v1` to the loop preheader we know that we can also
        // hoist `v7 = mul v6, v0`.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            jmp b1(i32 0)
          b1(v2: i32):
            v5 = lt v2, i32 4
            jmpif v5 then: b3, else: b2
          b2():
            return
          b3():
            v6 = mul v0, v1
            v7 = mul v6, v0
            v8 = eq v7, i32 12
            constrain v7 == i32 12
            v9 = unchecked_add v2, i32 1
            jmp b1(v9)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let main = ssa.main();

        let instructions = main.dfg[main.entry_block()].instructions();
        assert_eq!(instructions.len(), 0); // The final return is not counted

        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            v3 = mul v0, v1
            v4 = mul v3, v0
            v6 = eq v4, i32 12
            constrain v4 == i32 12
            jmp b1(i32 0)
          b1(v2: i32):
            v9 = lt v2, i32 4
            jmpif v9 then: b3, else: b2
          b2():
            return
          b3():
            v11 = unchecked_add v2, i32 1
            jmp b1(v11)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_hoist_instructions_with_side_effects() {
        // In `v12 = load v5` in `b3`, `v5` is defined outside the loop.
        // However, as the instruction has side effects, we want to make sure
        // we do not hoist the instruction to the loop preheader.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            v4 = make_array [u32 0, u32 0, u32 0, u32 0, u32 0] : [u32; 5]
            inc_rc v4
            v5 = allocate -> &mut [u32; 5]
            store v4 at v5
            jmp b1(u32 0)
          b1(v2: u32):
            v7 = lt v2, u32 4
            jmpif v7 then: b3, else: b2
          b2():
            v12 = load v5 -> [u32; 5]
            v14 = array_get v12, index u32 2 -> u32
            constrain v14 == u32 3
            return
          b3():
            v8 = load v5 -> [u32; 5]
            v9 = array_set v8, index v0, value v1
            store v9 at v5
            v11 = unchecked_add v2, u32 1
            jmp b1(v11)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let main = ssa.main();

        let instructions = main.dfg[main.entry_block()].instructions();
        assert_eq!(instructions.len(), 4); // The final return is not counted

        let ssa = ssa.loop_invariant_code_motion();
        // The code should be unchanged
        assert_normalized_ssa_equals(ssa, src);
    }

    #[test]
    fn hoist_array_gets_using_induction_variable_with_const_bound() {
        // SSA for the following program:
        //
        // fn triple_loop(x: u32) {
        //   let arr = [2; 5];
        //   for i in 0..4 {
        //       for j in 0..4 {
        //           for _ in 0..4 {
        //               assert_eq(arr[i], x);
        //               assert_eq(arr[j], x);
        //           }
        //       }
        //   }
        // }
        //
        // `arr[i]` and `arr[j]` are safe to hoist as we know the maximum possible index
        // to be used for both array accesses.
        // We want to make sure `arr[i]` is hoisted to the outermost loop body and that
        // `arr[j]` is hoisted to the second outermost loop body.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            v6 = make_array [u32 2, u32 2, u32 2, u32 2, u32 2] : [u32; 5]
            inc_rc v6
            jmp b1(u32 0)
          b1(v2: u32):
            v9 = lt v2, u32 4
            jmpif v9 then: b3, else: b2
          b2():
            return
          b3():
            jmp b4(u32 0)
          b4(v3: u32):
            v10 = lt v3, u32 4
            jmpif v10 then: b6, else: b5
          b5():
            v12 = unchecked_add v2, u32 1
            jmp b1(v12)
          b6():
            jmp b7(u32 0)
          b7(v4: u32):
            v13 = lt v4, u32 4
            jmpif v13 then: b9, else: b8
          b8():
            v14 = unchecked_add v3, u32 1
            jmp b4(v14)
          b9():
            v15 = array_get v6, index v2 -> u32
            v16 = eq v15, v0
            constrain v15 == v0
            v17 = array_get v6, index v3 -> u32
            v18 = eq v17, v0
            constrain v17 == v0
            v19 = unchecked_add v4, u32 1
            jmp b7(v19)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            v6 = make_array [u32 2, u32 2, u32 2, u32 2, u32 2] : [u32; 5]
            inc_rc v6
            jmp b1(u32 0)
          b1(v2: u32):
            v9 = lt v2, u32 4
            jmpif v9 then: b3, else: b2
          b2():
            return
          b3():
            v10 = array_get v6, index v2 -> u32
            v11 = eq v10, v0
            constrain v10 == v0
            jmp b4(u32 0)
          b4(v3: u32):
            v12 = lt v3, u32 4
            jmpif v12 then: b6, else: b5
          b5():
            v19 = unchecked_add v2, u32 1
            jmp b1(v19)
          b6():
            v13 = array_get v6, index v3 -> u32
            v14 = eq v13, v0
            constrain v13 == v0
            jmp b7(u32 0)
          b7(v4: u32):
            v15 = lt v4, u32 4
            jmpif v15 then: b9, else: b8
          b8():
            v18 = unchecked_add v3, u32 1
            jmp b4(v18)
          b9():
            v17 = unchecked_add v4, u32 1
            jmp b7(v17)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn insert_inc_rc_when_moving_make_array() {
        // SSA for the following program:
        //
        // unconstrained fn main(x: u32, y: u32) {
        //   let mut a1 = [1, 2, 3, 4, 5];
        //   a1[x] = 64;
        //   for i in 0 .. 5 {
        //       let mut a2 = [1, 2, 3, 4, 5];
        //       a2[y + i] = 128;
        //       foo(a2);
        //   }
        //   foo(a1);
        // }
        //
        // We want to make sure move a loop invariant make_array instruction,
        // to account for whether that array has been marked as mutable.
        // To do so, we increment the reference counter on the array we are moving.
        // In the SSA below, we want to move `v42` out of the loop.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            v8 = make_array [Field 1, Field 2, Field 3, Field 4, Field 5] : [Field; 5]
            v9 = allocate -> &mut [Field; 5]
            v11 = array_set v8, index v0, value Field 64
            v13 = add v0, u32 1
            store v11 at v9
            jmp b1(u32 0)
          b1(v2: u32):
            v16 = lt v2, u32 5
            jmpif v16 then: b3, else: b2
          b2():
            v17 = load v9 -> [Field; 5]
            call f1(v17)
            return
          b3():
            v19 = make_array [Field 1, Field 2, Field 3, Field 4, Field 5] : [Field; 5]
            v20 = allocate -> &mut [Field; 5]
            v21 = add v1, v2
            v23 = array_set v19, index v21, value Field 128
            call f1(v23)
            v24 = unchecked_add v2, u32 1
            jmp b1(v24)
        }
        brillig(inline) fn foo f1 {
          b0(v0: [Field; 5]):
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        // We expect the `make_array` at the top of `b3` to be replaced with an `inc_rc`
        // of the newly hoisted `make_array` at the end of `b0`.
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            v8 = make_array [Field 1, Field 2, Field 3, Field 4, Field 5] : [Field; 5]
            v9 = allocate -> &mut [Field; 5]
            v11 = array_set v8, index v0, value Field 64
            v13 = add v0, u32 1
            store v11 at v9
            v14 = make_array [Field 1, Field 2, Field 3, Field 4, Field 5] : [Field; 5]
            jmp b1(u32 0)
          b1(v2: u32):
            v17 = lt v2, u32 5
            jmpif v17 then: b3, else: b2
          b2():
            v24 = load v9 -> [Field; 5]
            call f1(v24)
            return
          b3():
            inc_rc v14
            v18 = allocate -> &mut [Field; 5]
            v19 = add v1, v2
            v21 = array_set v14, index v19, value Field 128
            call f1(v21)
            v23 = unchecked_add v2, u32 1
            jmp b1(v23)
        }
        brillig(inline) fn foo f1 {
          b0(v0: [Field; 5]):
            return
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn transform_safe_ops_to_unchecked_during_code_motion() {
        // This test is identical to `simple_loop_invariant_code_motion`, except this test
        // uses a checked add in `b3`.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
              jmp b1(i32 0)
          b1(v2: i32):
              v5 = lt v2, i32 4
              jmpif v5 then: b3, else: b2
          b2():
              return
          b3():
              v6 = mul v0, v1
              constrain v6 == i32 6
              v8 = add v2, i32 1
              jmp b1(v8)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        // `v8 = add v2, i32 1` in b3 should now be `v9 = unchecked_add v2, i32 1` in b3
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: i32, v1: i32):
            v3 = mul v0, v1
            constrain v3 == i32 6
            jmp b1(i32 0)
          b1(v2: i32):
            v7 = lt v2, i32 4
            jmpif v7 then: b3, else: b2
          b2():
            return
          b3():
            v9 = unchecked_add v2, i32 1
            jmp b1(v9)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_transform_unsafe_sub_to_unchecked() {
        // This test is identical to `simple_loop_invariant_code_motion`, except this test
        // uses a checked sub in `b3`.
        // We want to make sure that our sub operation has the induction variable (`v2`) on the lhs.
        // The induction variable `v2` is placed on the lhs of the sub operation
        // to test that we are checking against the loop's lower bound
        // rather than the upper bound (add/mul only check against the upper bound).
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            jmp b1(u32 0)
          b1(v2: u32):
            v5 = lt v2, u32 4
            jmpif v5 then: b3, else: b2
          b2():
            return
          b3():
            v7 = sub v2, u32 1
            jmp b1(v7)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, src);
    }

    #[test]
    fn transform_safe_sub_to_unchecked() {
        // This test is identical to `do_not_transform_unsafe_sub_to_unchecked`, except the loop
        // in this test starts with a lower bound of `1`.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
              jmp b1(u32 1)
          b1(v2: u32):
              v5 = lt v2, u32 4
              jmpif v5 then: b3, else: b2
          b2():
              return
          b3():
              v8 = sub v2, u32 1
              jmp b1(v8)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        // `v8 = sub v2, u32 1` in b3 should now be `v9 = unchecked_sub v2, u32 1` in b3
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: u32, v1: u32):
            jmp b1(u32 1)
          b1(v2: u32):
            v5 = lt v2, u32 4
            jmpif v5 then: b3, else: b2
          b2():
            return
          b3():
            v6 = unchecked_sub v2, u32 1
            jmp b1(v6)
        }
        ";

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_hoist_unsafe_div() {
        // This test is similar to `nested_loop_invariant_code_motion`, except that
        // the loop logic is under a dynamic predicate.
        // Divisions are only reliant upon predicates and do not have other side effects.
        //
        // If an unsafe division occurs in a loop block that is not control dependent,
        // we can still safely hoist that division as that instruction is always going to be hit.
        // Thus, we place the unsafe division under a predicate to ensure that we are testing
        // division hoisting based upon loop bounds and nothing else.
        //
        // The operation in question we are trying to hoist is `v12 = div u32 10, v1`.
        // Check whether the lower bound of the outer loop is zero and that we do not
        // hoist an operation that can potentially error with a division by zero.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32):
            v4 = eq v0, u32 5
            jmp b1(u32 0)
          b1(v1: u32):
            v7 = lt v1, u32 4
            jmpif v7 then: b2, else: b3
          b2():
            jmp b4(u32 0)
          b3():
            return
          b4(v2: u32):
            v8 = lt v2, u32 4
            jmpif v8 then: b5, else: b6
          b5():
            jmpif v4 then: b7, else: b8
          b6():
            v10 = unchecked_add v1, u32 1
            jmp b1(v10)
          b7():
            v12 = div u32 10, v1
            constrain v12 == u32 6
            jmp b8()
          b8():
            v14 = unchecked_add v2, u32 1
            jmp b4(v14)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, src);
    }

    #[test]
    fn hoist_safe_div() {
        // This test is identical to `do_not_hoist_unsafe_div`, except the loop
        // in this test starts with a lower bound of `1`.
        let src = "
        brillig(inline) fn main f0 {
          b0(v0: u32):
            v4 = eq v0, u32 5
            jmp b1(u32 1)
          b1(v1: u32):
            v7 = lt v1, u32 4
            jmpif v7 then: b2, else: b3
          b2():
            jmp b4(u32 0)
          b3():
            return
          b4(v2: u32):
            v9 = lt v2, u32 4
            jmpif v9 then: b5, else: b6
          b5():
            jmpif v4 then: b7, else: b8
          b6():
            v10 = unchecked_add v1, u32 1
            jmp b1(v10)
          b7():
            v12 = div u32 10, v1
            constrain v12 == u32 6
            jmp b8()
          b8():
            v14 = unchecked_add v2, u32 1
            jmp b4(v14)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();
        let expected = "
        brillig(inline) fn main f0 {
          b0(v0: u32):
            v4 = eq v0, u32 5
            jmp b1(u32 1)
          b1(v1: u32):
            v7 = lt v1, u32 4
            jmpif v7 then: b2, else: b3
          b2():
            v9 = div u32 10, v1
            jmp b4(u32 0)
          b3():
            return
          b4(v2: u32):
            v11 = lt v2, u32 4
            jmpif v11 then: b5, else: b6
          b5():
            jmpif v4 then: b7, else: b8
          b6():
            v12 = unchecked_add v1, u32 1
            jmp b1(v12)
          b7():
            constrain v9 == u32 6
            jmp b8()
          b8():
            v14 = unchecked_add v2, u32 1
            jmp b4(v14)
        }
        ";

        assert_normalized_ssa_equals(ssa, expected);
    }
}

#[cfg(test)]
mod control_dependence {
    use crate::ssa::{opt::assert_normalized_ssa_equals, ssa_gen::Ssa};

    #[test]
    fn do_not_hoist_unsafe_mul_in_control_dependent_block() {
        let src = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            v4 = eq v0, u32 5
            jmp loop(u32 0)
          loop(v2: u32):
            v7 = lt v2, u32 4
            jmpif v7 then: loop_cond, else: exit
          loop_cond():
            jmpif v4 then: loop_body, else: loop_end
          exit():
            return
          loop_body():
            v8 = mul v0, v1
            constrain v8 == u32 12
            jmp loop_end()
          loop_end():
            v11 = unchecked_add v2, u32 1
            jmp loop(v11)
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();
        assert_normalized_ssa_equals(ssa, src);
    }

    #[test]
    fn hoist_safe_mul_that_is_non_control_dependent() {
        let src = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            jmp loop(u32 0)
          loop(v2: u32):
            v3 = lt v2, u32 4
            jmpif v3 then: loop_body, else: exit
          loop_body():
            v6 = mul v0, v1
            v7 = mul v6, v0
            constrain v7 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";
        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.loop_invariant_code_motion();

        let expected = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            v3 = mul v0, v1
            v4 = mul v3, v0
            constrain v4 == u32 12
            jmp loop(u32 0)
          loop(v2: u32):
            v8 = lt v2, u32 4
            jmpif v8 then: loop_body, else: exit
          loop_body():
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn non_control_dependent_loop_follows_control_dependent_loop() {
        // Test that we appropriately reset the control dependence status.
        // This program first has a loop with a control dependent body, thus preventing hoisting instructions.
        // There is then a separate second loop which is non control dependent for which
        // we expect instructions to be hoisted.
        let src = "
      brillig(inline) fn main f0 {
        entry(v0: u32, v1: u32):
          v5 = eq v0, u32 5
          jmp loop_1(u32 0)
        loop_1(v2: u32):
          v8 = lt v2, u32 4
          jmpif v8 then: loop_1_cond, else: loop_1_exit
        loop_1_cond():
          jmpif v5 then: loop_1_body, else: loop_1_end
        loop_1_exit():
          jmp loop_2(u32 0)
        loop_1_body():
          v15 = mul v0, v1
          constrain v15 == u32 12
          jmp loop_1_end()
        loop_1_end():
          v16 = unchecked_add v2, u32 1
          jmp loop_1(v16)
        loop_2(v3: u32):
          v10 = lt v3, u32 4
          jmpif v10 then: loop_2_body, else: exit
        loop_2_body():
          v9 = mul v0, v1
          v11 = mul v9, v0
          constrain v11 == u32 12
          v14 = unchecked_add v3, u32 1
          jmp loop_2(v14)
        exit():
          return
      }
      ";

        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.loop_invariant_code_motion();

        // From loop_2_body:
        // ```
        // v9 = mul v0, v1
        // v11 = mul v9, v0
        // constrain v11 == u32 12
        // ```
        // To loop_1_exit:
        // ```
        // v9 = mul v0, v1
        // v10 = mul v9, v0
        // constrain v10 == u32 12
        // ```
        let expected = "
      brillig(inline) fn main f0 {
        entry(v0: u32, v1: u32):
          v5 = eq v0, u32 5
          jmp loop_1(u32 0)
        loop_1(v2: u32):
          v8 = lt v2, u32 4
          jmpif v8 then: loop_1_cond, else: loop_1_exit
        loop_1_cond():
          jmpif v5 then: loop_1_body, else: loop_1_end
        loop_1_exit():
          v9 = mul v0, v1
          v10 = mul v9, v0
          constrain v10 == u32 12
          jmp loop_2(u32 0)
        loop_1_body():
          v15 = mul v0, v1
          constrain v15 == u32 12
          jmp loop_1_end()
        loop_1_end():
          v16 = unchecked_add v2, u32 1
          jmp loop_1(v16)
        loop_2(v3: u32):
          v12 = lt v3, u32 4
          jmpif v12 then: loop_2_body, else: exit
        loop_2_body():
          v14 = unchecked_add v3, u32 1
          jmp loop_2(v14)
        exit():
          return
      }
      ";

        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_hoist_constrain_in_loop_with_zero_upper_bound() {
        // This test is the same as `hoist_safe_mul_that_is_non_control_dependent` except
        // that the upper loop bound is zero
        let src = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            jmp loop(u32 0)
          loop(v2: u32):
            v3 = lt v2, u32 0
            jmpif v3 then: loop_body, else: exit
          loop_body():
            v6 = mul v0, v1
            v7 = mul v6, v0
            constrain v7 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();

        // We expect the constrain to remain inside of `loop_body`
        // as the loop is never going to be executed.
        // If the constrain were to be hoisted out it could potentially
        // cause the program to fail when it is not meant to fail.
        let expected = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            v3 = mul v0, v1
            v4 = mul v3, v0
            jmp loop(u32 0)
          loop(v2: u32):
            jmpif u1 0 then: loop_body, else: exit
          loop_body():
            constrain v4 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_hoist_constrain_in_loop_with_equal_non_zero_loop_bounds() {
        // This test is the same as `hoist_safe_mul_that_is_non_control_dependent` except
        // that the lower and upper loop bounds are the same and greater than zero
        let src = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            jmp loop(u32 1)
          loop(v2: u32):
            v3 = lt v2, u32 1
            jmpif v3 then: loop_body, else: exit
          loop_body():
            v6 = mul v0, v1
            v7 = mul v6, v0
            constrain v7 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();
        // We expect the constrain to remain inside of `loop_body`
        // as the loop is never going to be executed.
        // If the constrain were to be hoisted out it could potentially
        // cause the program to fail when it is not meant to fail.
        let expected = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            v3 = mul v0, v1
            v4 = mul v3, v0
            jmp loop(u32 1)
          loop(v2: u32):
            v7 = eq v2, u32 0
            jmpif v7 then: loop_body, else: exit
          loop_body():
            constrain v4 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        assert_normalized_ssa_equals(ssa, expected);
    }

    #[test]
    fn do_not_hoist_constrain_in_loop_with_dynamic_upper_bound() {
        // This test is the same as `hoist_safe_mul_that_is_non_control_dependent` except
        // that the upper loop bound is dynamic
        let src = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            jmp loop(u32 0)
          loop(v2: u32):
            v3 = lt v2, v1
            jmpif v3 then: loop_body, else: exit
          loop_body():
            v6 = mul v0, v1
            v7 = mul v6, v0
            constrain v7 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();

        let ssa = ssa.loop_invariant_code_motion();

        // We expect the constrain to remain inside of `loop_body`
        // as that block may potentially never be executed.
        // If the constrain were to be hoisted out it could potentially
        // cause the program to fail when it is not meant to fail.
        let expected = "
        brillig(inline) fn main f0 {
          entry(v0: u32, v1: u32):
            v3 = mul v0, v1
            v4 = mul v3, v0
            jmp loop(u32 0)
          loop(v2: u32):
            v6 = lt v2, v1
            jmpif v6 then: loop_body, else: exit
          loop_body():
            constrain v4 == u32 12
            v10 = unchecked_add v2, u32 1
            jmp loop(v10)
          exit():
            return
        }
        ";

        assert_normalized_ssa_equals(ssa, expected);
    }
}
