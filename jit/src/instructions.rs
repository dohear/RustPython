use super::{JitCompileError, JitSig, JitType};
use cranelift::codegen::ir::FuncRef;
use cranelift::prelude::*;
use num_traits::cast::ToPrimitive;
use rustpython_compiler_core::bytecode::{
    self, BinaryOperator, BorrowedConstant, CodeObject, ComparisonOperator, Instruction, Label,
    OpArg, OpArgState, UnaryOperator,
};
use std::collections::HashMap;


#[repr(u16)]
enum CustomTrapCode {
    /// Raised when shifting by a negative number
    NegativeShiftCount = 1,
}

#[derive(Clone)]
struct Local {
    var: Variable,
    ty: JitType,
}

#[derive(Debug)]
enum JitValue {
    Int(Value),
    Float(Value),
    Bool(Value),
    None,
    Tuple(Vec<JitValue>),
    FuncRef(FuncRef),
}

impl JitValue {
    fn from_type_and_value(ty: JitType, val: Value) -> JitValue {
        match ty {
            JitType::Int => JitValue::Int(val),
            JitType::Float => JitValue::Float(val),
            JitType::Bool => JitValue::Bool(val),
        }
    }

    fn to_jit_type(&self) -> Option<JitType> {
        match self {
            JitValue::Int(_) => Some(JitType::Int),
            JitValue::Float(_) => Some(JitType::Float),
            JitValue::Bool(_) => Some(JitType::Bool),
            JitValue::None | JitValue::Tuple(_) | JitValue::FuncRef(_) => None,
        }
    }

    fn into_value(self) -> Option<Value> {
        match self {
            JitValue::Int(val) | JitValue::Float(val) | JitValue::Bool(val) => Some(val),
            JitValue::None | JitValue::Tuple(_) | JitValue::FuncRef(_) => None,
        }
    }
}

pub struct FunctionCompiler<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    stack: Vec<JitValue>,
    variables: Box<[Option<Local>]>,
    label_to_block: HashMap<Label, Block>,
    pub(crate) sig: JitSig,
}

impl<'a, 'b> FunctionCompiler<'a, 'b> {
    pub fn new(
        builder: &'a mut FunctionBuilder<'b>,
        num_variables: usize,
        arg_types: &[JitType],
        ret_type: Option<JitType>,
        entry_block: Block,
    ) -> FunctionCompiler<'a, 'b> {
        let mut compiler = FunctionCompiler {
            builder,
            stack: Vec::new(),
            variables: vec![None; num_variables].into_boxed_slice(),
            label_to_block: HashMap::new(),
            sig: JitSig {
                args: arg_types.to_vec(),
                ret: ret_type,
            },
        };
        let params = compiler.builder.func.dfg.block_params(entry_block).to_vec();
        for (i, (ty, val)) in arg_types.iter().zip(params).enumerate() {
            compiler
                .store_variable(i as u32, JitValue::from_type_and_value(ty.clone(), val))
                .unwrap();
        }
        compiler
    }

    fn pop_multiple(&mut self, count: usize) -> Vec<JitValue> {
        let stack_len = self.stack.len();
        self.stack.drain(stack_len - count..).collect()
    }

    fn store_variable(
        &mut self,
        idx: bytecode::NameIdx,
        val: JitValue,
    ) -> Result<(), JitCompileError> {
        let builder = &mut self.builder;
        let ty = val.to_jit_type().ok_or(JitCompileError::NotSupported)?;
        let local = self.variables[idx as usize].get_or_insert_with(|| {
            let var = Variable::new(idx as usize);
            let local = Local {
                var,
                ty: ty.clone(),
            };
            builder.declare_var(var, ty.to_cranelift());
            local
        });
        if ty != local.ty {
            Err(JitCompileError::NotSupported)
        } else {
            self.builder.def_var(local.var, val.into_value().unwrap());
            Ok(())
        }
    }

    fn boolean_val(&mut self, val: JitValue) -> Result<Value, JitCompileError> {
        match val {
            JitValue::Float(val) => {
                let zero = self.builder.ins().f64const(0.0);
                let val = self.builder.ins().fcmp(FloatCC::NotEqual, val, zero);
                Ok(val)
            }
            JitValue::Int(val) => {
                let zero = self.builder.ins().iconst(types::I64, 0);
                let val = self.builder.ins().icmp(IntCC::NotEqual, val, zero);
                Ok(val)
            }
            JitValue::Bool(val) => Ok(val),
            JitValue::None => Ok(self.builder.ins().iconst(types::I8, 0)),
            JitValue::Tuple(_) | JitValue::FuncRef(_) => Err(JitCompileError::NotSupported),
        }
    }

    fn get_or_create_block(&mut self, label: Label) -> Block {
        let builder = &mut self.builder;
        *self
            .label_to_block
            .entry(label)
            .or_insert_with(|| builder.create_block())
    }

    pub fn compile<C: bytecode::Constant>(
        &mut self,
        func_ref: FuncRef,
        bytecode: &CodeObject<C>,
    ) -> Result<(), JitCompileError> {
        let label_targets = bytecode.label_targets();
        let mut arg_state = OpArgState::default();

        // Track whether we have "returned" in the current block
        let mut in_unreachable_code = false;

        for (offset, &raw_instr) in bytecode.instructions.iter().enumerate() {
            let label = Label(offset as u32);
            let (instruction, arg) = arg_state.get(raw_instr);

            // If this is a label that some earlier jump can target,
            // treat it as the start of a new reachable block:
            if label_targets.contains(&label) {
                // Create or get the block for this label:
                let target_block = self.get_or_create_block(label);

                // If the current block isn't terminated, jump:
                if let Some(cur) = self.builder.current_block() {
                    if cur != target_block && self.builder.func.layout.last_inst(cur).is_none() {
                        self.builder.ins().jump(target_block, &[]);
                    }
                }
                // Switch to the target block
                if self.builder.current_block() != Some(target_block) {
                    self.builder.switch_to_block(target_block);
                }

                // We are definitely reachable again at this label
                in_unreachable_code = false;
            }

            // If we're in unreachable code, skip this instruction unless the label re-entered above.
            if in_unreachable_code {
                continue;
            }

            // Actually compile this instruction:
            self.add_instruction(func_ref, bytecode, instruction, arg)?;

            // If that was a return instruction, mark future instructions unreachable
            match instruction {
                Instruction::ReturnValue | Instruction::ReturnConst { .. } => {
                    in_unreachable_code = true;
                }
                _ => {}
            }
        }

        // After processing, if the current block is unterminated, insert a trap or fallthrough
        if let Some(cur) = self.builder.current_block() {
            if self.builder.func.layout.last_inst(cur).is_none() {
                self.builder.ins().trap(TrapCode::user(0).unwrap());
            }
        }
        Ok(())
    }

    fn prepare_const<C: bytecode::Constant>(
        &mut self,
        constant: BorrowedConstant<C>,
    ) -> Result<JitValue, JitCompileError> {
        let value = match constant {
            BorrowedConstant::Integer { value } => {
                let val = self.builder.ins().iconst(
                    types::I64,
                    value.to_i64().ok_or(JitCompileError::NotSupported)?,
                );
                JitValue::Int(val)
            }
            BorrowedConstant::Float { value } => {
                let val = self.builder.ins().f64const(value);
                JitValue::Float(val)
            }
            BorrowedConstant::Boolean { value } => {
                let val = self.builder.ins().iconst(types::I8, value as i64);
                JitValue::Bool(val)
            }
            BorrowedConstant::None => JitValue::None,
            _ => return Err(JitCompileError::NotSupported),
        };
        Ok(value)
    }

    fn return_value(&mut self, val: JitValue) -> Result<(), JitCompileError> {
        if let Some(ref ty) = self.sig.ret {
            // If the signature has a return type, enforce it
            if val.to_jit_type().as_ref() != Some(ty) {
                return Err(JitCompileError::NotSupported);
            }
        } else {
            // First time we see a return, define it in the signature
            let ty = val.to_jit_type().ok_or(JitCompileError::NotSupported)?;
            self.sig.ret = Some(ty.clone());
            self.builder
                .func
                .signature
                .returns
                .push(AbiParam::new(ty.to_cranelift()));
        }

        // If this is e.g. an Int, Float, or Bool we have a Cranelift `Value`.
        // If we have JitValue::None or .Tuple(...) but can't handle that, error out (or handle differently).
        let cr_val = val.into_value().ok_or(JitCompileError::NotSupported)?;

        self.builder.ins().return_(&[cr_val]);
        Ok(())
    }

    pub fn add_instruction<C: bytecode::Constant>(
        &mut self,
        func_ref: FuncRef,
        bytecode: &CodeObject<C>,
        instruction: Instruction,
        arg: OpArg,
    ) -> Result<(), JitCompileError> {
        match instruction {
            Instruction::ExtendedArg => Ok(()),
            Instruction::JumpIfFalse { target } => {
                let cond = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let val = self.boolean_val(cond)?;
                let then_block = self.get_or_create_block(target.get(arg));
                let else_block = self.builder.create_block();

                self.builder.ins().brif(val, else_block, &[], then_block, &[]);
                self.builder.switch_to_block(else_block);

                Ok(())
            }
            Instruction::JumpIfTrue { target } => {
                let cond = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let val = self.boolean_val(cond)?;
                let then_block = self.get_or_create_block(target.get(arg));
                let else_block = self.builder.create_block();

                self.builder.ins().brif(val, then_block, &[], else_block, &[]);
                self.builder.switch_to_block(else_block);

                Ok(())
            }

            Instruction::Jump { target } => {
                let target_block = self.get_or_create_block(target.get(arg));
                self.builder.ins().jump(target_block, &[]);
                Ok(())
            }
            Instruction::LoadFast(idx) => {
                let local = self.variables[idx.get(arg) as usize]
                    .as_ref()
                    .ok_or(JitCompileError::BadBytecode)?;
                self.stack.push(JitValue::from_type_and_value(
                    local.ty.clone(),
                    self.builder.use_var(local.var),
                ));
                Ok(())
            }
            Instruction::StoreFast(idx) => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                self.store_variable(idx.get(arg), val)
            }
            Instruction::LoadConst { idx } => {
                let val = self
                    .prepare_const(bytecode.constants[idx.get(arg) as usize].borrow_constant())?;
                self.stack.push(val);
                Ok(())
            }
            Instruction::BuildTuple { size } => {
                let elements = self.pop_multiple(size.get(arg) as usize);
                self.stack.push(JitValue::Tuple(elements));
                Ok(())
            }
            Instruction::UnpackSequence { size } => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let elements = match val {
                    JitValue::Tuple(elements) => elements,
                    _ => return Err(JitCompileError::NotSupported),
                };

                if elements.len() != size.get(arg) as usize {
                    return Err(JitCompileError::NotSupported);
                }

                self.stack.extend(elements.into_iter().rev());
                Ok(())
            }
            Instruction::ReturnValue => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                self.return_value(val)
            }
            Instruction::ReturnConst { idx } => {
                let val = self
                    .prepare_const(bytecode.constants[idx.get(arg) as usize].borrow_constant())?;
                self.return_value(val)
            }
            Instruction::CompareOperation { op, .. } => {
                let op = op.get(arg);
                // the rhs is popped off first
                let b = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let a_type: Option<JitType> = a.to_jit_type();
                let b_type: Option<JitType> = b.to_jit_type();

                match (a, b) {
                    (JitValue::Int(a), JitValue::Int(b))
                    | (JitValue::Bool(a), JitValue::Bool(b))
                    | (JitValue::Bool(a), JitValue::Int(b))
                    | (JitValue::Int(a), JitValue::Bool(b)) => {
                        let operand_one = match a_type.unwrap() {
                            JitType::Bool => self.builder.ins().uextend(types::I64, a),
                            _ => a,
                        };

                        let operand_two = match b_type.unwrap() {
                            JitType::Bool => self.builder.ins().uextend(types::I64, b),
                            _ => b,
                        };

                        let cond = match op {
                            ComparisonOperator::Equal => IntCC::Equal,
                            ComparisonOperator::NotEqual => IntCC::NotEqual,
                            ComparisonOperator::Less => IntCC::SignedLessThan,
                            ComparisonOperator::LessOrEqual => IntCC::SignedLessThanOrEqual,
                            ComparisonOperator::Greater => IntCC::SignedGreaterThan,
                            ComparisonOperator::GreaterOrEqual => IntCC::SignedGreaterThanOrEqual,
                        };

                        let val = self.builder.ins().icmp(cond, operand_one, operand_two);
                        self.stack.push(JitValue::Bool(val));
                        Ok(())
                    }
                    (JitValue::Float(a), JitValue::Float(b)) => {
                        let cond = match op {
                            ComparisonOperator::Equal => FloatCC::Equal,
                            ComparisonOperator::NotEqual => FloatCC::NotEqual,
                            ComparisonOperator::Less => FloatCC::LessThan,
                            ComparisonOperator::LessOrEqual => FloatCC::LessThanOrEqual,
                            ComparisonOperator::Greater => FloatCC::GreaterThan,
                            ComparisonOperator::GreaterOrEqual => FloatCC::GreaterThanOrEqual,
                        };

                        let val = self.builder.ins().fcmp(cond, a, b);
                        self.stack.push(JitValue::Bool(val));
                        Ok(())
                    }
                    _ => Err(JitCompileError::NotSupported),
                }
            }
            Instruction::UnaryOperation { op, .. } => {
                let op = op.get(arg);
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                match (op, a) {
                    (UnaryOperator::Minus, JitValue::Int(val)) => {
                        // Compile minus as 0 - a.
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let out = self.compile_sub(zero, val);
                        self.stack.push(JitValue::Int(out));
                        Ok(())
                    }
                    (UnaryOperator::Plus, JitValue::Int(val)) => {
                        // Nothing to do
                        self.stack.push(JitValue::Int(val));
                        Ok(())
                    }
                    (UnaryOperator::Not, a) => {
                        let boolean = self.boolean_val(a)?;
                        let not_boolean = self.builder.ins().bxor_imm(boolean, 1);
                        self.stack.push(JitValue::Bool(not_boolean));
                        Ok(())
                    }
                    _ => Err(JitCompileError::NotSupported),
                }
            }
            Instruction::BinaryOperation { op } | Instruction::BinaryOperationInplace { op } => {
                let op = op.get(arg);
                // the rhs is popped off first
                let b = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let a_type = a.to_jit_type();
                let b_type = b.to_jit_type();

                let val = match (op, a, b) {
                    (BinaryOperator::Add, JitValue::Int(a), JitValue::Int(b)) => {
                        let (out, carry) = self.builder.ins().sadd_overflow(a, b);
                        self.builder.ins().trapnz(carry, TrapCode::INTEGER_OVERFLOW);
                        JitValue::Int(out)
                    }
                    (BinaryOperator::Subtract, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.compile_sub(a, b))
                    }
                    (BinaryOperator::FloorDivide, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().sdiv(a, b))
                    }
                    (BinaryOperator::Multiply, JitValue::Int(a), JitValue::Int(b)) =>{
                        JitValue::Int(self.builder.ins().imul(a, b))
                    }
                    (BinaryOperator::Modulo, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().srem(a, b))
                    }
                    (BinaryOperator::Power, JitValue::Int(a), JitValue::Int(b)) => { 
                        JitValue::Int(self.compile_ipow(a, b)) 
                    }
                    (
                        BinaryOperator::Lshift | BinaryOperator::Rshift,
                        JitValue::Int(a),
                        JitValue::Int(b),
                    ) => {
                        // Shifts throw an exception if we have a negative shift count
                        // Remove all bits except the sign bit, and trap if its 1 (i.e. negative).
                        let sign = self.builder.ins().ushr_imm(b, 63);
                        self.builder.ins().trapnz(
                            sign,
                            TrapCode::user(CustomTrapCode::NegativeShiftCount as u8).unwrap(),
                        );

                        let out = if op == BinaryOperator::Lshift {
                            self.builder.ins().ishl(a, b)
                        } else {
                            self.builder.ins().sshr(a, b)
                        };
                        JitValue::Int(out)
                    }
                    (BinaryOperator::And, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().band(a, b))
                    }
                    (BinaryOperator::Or, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().bor(a, b))
                    }
                    (BinaryOperator::Xor, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().bxor(a, b))
                    }

                    // Floats
                    (BinaryOperator::Add, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fadd(a, b))
                    }
                    (BinaryOperator::Subtract, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fsub(a, b))
                    }
                    (BinaryOperator::Multiply, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fmul(a, b))
                    }
                    (BinaryOperator::Divide, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fdiv(a, b))
                    }
                    (BinaryOperator::Power, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fmul(a, b))
                    }

                    // Floats and Integers
                    (_, JitValue::Int(a), JitValue::Float(b))
                    | (_, JitValue::Float(a), JitValue::Int(b)) => {
                        let operand_one = match a_type.unwrap() {
                            JitType::Int => self.builder.ins().fcvt_from_sint(types::F64, a),
                            _ => a,
                        };

                        let operand_two = match b_type.unwrap() {
                            JitType::Int => self.builder.ins().fcvt_from_sint(types::F64, b),
                            _ => b,
                        };

                        match op {
                            BinaryOperator::Add => {
                                JitValue::Float(self.builder.ins().fadd(operand_one, operand_two))
                            }
                            BinaryOperator::Subtract => {
                                JitValue::Float(self.builder.ins().fsub(operand_one, operand_two))
                            }
                            BinaryOperator::Multiply => {
                                JitValue::Float(self.builder.ins().fmul(operand_one, operand_two))
                            }
                            BinaryOperator::Divide => {
                                JitValue::Float(self.builder.ins().fdiv(operand_one, operand_two))
                            }
                            BinaryOperator::Power => {
                                JitValue::Float(self.builder.ins().fmul(operand_one, operand_two))
                            }
                            _ => return Err(JitCompileError::NotSupported),
                        }
                    }
                    _ => return Err(JitCompileError::NotSupported),
                };
                self.stack.push(val);

                Ok(())
            }
            Instruction::SetupLoop { .. } | Instruction::PopBlock => {
                // TODO: block support
                Ok(())
            }
            Instruction::LoadGlobal(idx) => {
                let name = &bytecode.names[idx.get(arg) as usize];

                if name.as_ref() != bytecode.obj_name.as_ref() {
                    Err(JitCompileError::NotSupported)
                } else {
                    self.stack.push(JitValue::FuncRef(func_ref));
                    Ok(())
                }
            }
            Instruction::CallFunctionPositional { nargs } => {
                let nargs = nargs.get(arg);

                let mut args = Vec::new();
                for _ in 0..nargs {
                    let arg = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                    args.push(arg.into_value().unwrap());
                }

                match self.stack.pop().ok_or(JitCompileError::BadBytecode)? {
                    JitValue::FuncRef(reference) => {
                        let call = self.builder.ins().call(reference, &args);
                        let returns = self.builder.inst_results(call);
                        self.stack.push(JitValue::Int(returns[0]));

                        Ok(())
                    }
                    _ => Err(JitCompileError::BadBytecode),
                }
            }
            _ => Err(JitCompileError::NotSupported),
        }
    }

    fn compile_sub(&mut self, a: Value, b: Value) -> Value {
        let (out, carry) = self.builder.ins().ssub_overflow(a, b);
        self.builder
            .ins()
            .trapnz(carry, TrapCode::INTEGER_OVERFLOW);
        out
    }

    fn compile_ipow(&mut self, a: Value, b: Value) -> Value {

        let zero = self.builder.ins().iconst(types::I64, 0);
        let one_i64 = self.builder.ins().iconst(types::I64, 1);
        
        // Create required blocks
        let check_negative = self.builder.create_block();
        let handle_negative = self.builder.create_block();
        let loop_block = self.builder.create_block();
        let continue_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        
        // Set up block parameters
        self.builder.append_block_param(check_negative, types::I64);  // exponent
        self.builder.append_block_param(check_negative, types::I64);  // base
        
        self.builder.append_block_param(handle_negative, types::I64); // abs(exponent)
        self.builder.append_block_param(handle_negative, types::I64); // base
        
        self.builder.append_block_param(loop_block, types::I64);     // exponent
        self.builder.append_block_param(loop_block, types::I64);     // result
        self.builder.append_block_param(loop_block, types::I64);     // base
        
        self.builder.append_block_param(exit_block, types::I64);     // final result
    
        // Set up parameters for continue_block
        self.builder.append_block_param(continue_block, types::I64); // exponent
        self.builder.append_block_param(continue_block, types::I64); // result
        self.builder.append_block_param(continue_block, types::I64); // base
        
        // Initial jump to check if exponent is negative
        self.builder.ins().jump(check_negative, &[b, a]);
        
        // Check if exponent is negative
        self.builder.switch_to_block(check_negative);
        let params = self.builder.block_params(check_negative);
        let exp_check = params[0];
        let base_check = params[1];
        
        let is_negative = self.builder.ins().icmp(IntCC::SignedLessThan, exp_check, zero);
        //self.builder.ins().brnz(is_negative, handle_negative, &[exp_check, base_check]);
        //self.builder.ins().jump(loop_block, &[exp_check, one_i64, base_check]);
        self.builder.ins().brif(is_negative, handle_negative, &[exp_check, base_check], loop_block, &[exp_check, one_i64, base_check]);
        
        // Handle negative exponent (return 0 for integer exponentiation)
        self.builder.switch_to_block(handle_negative);
        self.builder.ins().jump(exit_block, &[zero]);  // Return 0 for negative exponents
    
        // Loop block logic (square-and-multiply algorithm)
        self.builder.switch_to_block(loop_block);
        let params = self.builder.block_params(loop_block);
        let exp_phi = params[0];    
        let result_phi = params[1]; 
        let base_phi = params[2];   
    
        // Check if exponent is zero
        let is_zero = self.builder.ins().icmp(IntCC::Equal, exp_phi, zero);
        //self.builder.ins().brnz(is_zero, exit_block, &[result_phi]);
        //self.builder.ins().jump(continue_block, &[exp_phi, result_phi, base_phi]);
        self.builder.ins().brif(is_zero, exit_block, &[result_phi], continue_block, &[exp_phi, result_phi, base_phi]);
    
        // Continue block for non-zero case
        self.builder.switch_to_block(continue_block);
        let params = self.builder.block_params(continue_block);
        let exp_phi = params[0];
        let result_phi = params[1];
        let base_phi = params[2];
        
        // If exponent is odd, multiply result by base
        let is_odd = self.builder.ins().band_imm(exp_phi, 1);
        let is_odd = self.builder.ins().icmp_imm(IntCC::Equal, is_odd, 1);
        let mul_result = self.builder.ins().imul(result_phi, base_phi);
        let new_result = self.builder.ins().select(is_odd, mul_result, result_phi);
        
        // Square the base and divide exponent by 2
        let squared_base = self.builder.ins().imul(base_phi, base_phi);
        let new_exp = self.builder.ins().sshr_imm(exp_phi, 1);
        self.builder.ins().jump(loop_block, &[new_exp, new_result, squared_base]);
    
        // Exit block
        self.builder.switch_to_block(exit_block);
        let res = self.builder.block_params(exit_block)[0];
    
        // Seal all blocks
        self.builder.seal_block(check_negative);
        self.builder.seal_block(handle_negative);
        self.builder.seal_block(loop_block);
        self.builder.seal_block(continue_block);
        self.builder.seal_block(exit_block);
    
        res 
    }

}
