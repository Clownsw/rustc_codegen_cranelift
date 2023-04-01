//! The interpret driver uses [`cranelift_interpret`] to interpret programs without writing any object
//! files.

use std::collections::BTreeMap;

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::Function;
use cranelift_interpreter::address::{Address, AddressRegion};
use cranelift_interpreter::instruction::DfgInstructionContext;
use cranelift_interpreter::interpreter::InterpreterError;
use cranelift_interpreter::step::{step, ControlFlow};
use rustc_codegen_ssa::CrateInfo;
use rustc_middle::mir::mono::MonoItem;
use rustc_span::Symbol;

use cranelift_interpreter::frame::Frame;
use cranelift_interpreter::state::{InterpreterFunctionRef, State};

use crate::{prelude::*, BackendConfig};

pub(crate) fn run_interpret(tcx: TyCtxt<'_>, backend_config: BackendConfig) -> ! {
    if !tcx.sess.opts.output_types.should_codegen() {
        tcx.sess.fatal("JIT mode doesn't work with `cargo check`");
    }

    if !tcx.sess.crate_types().contains(&rustc_session::config::CrateType::Executable) {
        tcx.sess.fatal("can't jit non-executable crate");
    }

    let mut interpret_module = super::lto::make_module(tcx.sess, &backend_config);

    for (_name, module) in super::lto::load_lto_modules(
        tcx,
        &CrateInfo::new(tcx, "dummy_target_cpu".to_string()),
        &backend_config,
    ) {
        module.apply_to(&mut interpret_module);
    }

    crate::allocator::codegen(tcx, &mut interpret_module);
    crate::main_shim::maybe_create_entry_wrapper(tcx, &mut interpret_module, true, true);

    let (_, cgus) = tcx.collect_and_partition_mono_items(());
    let mono_items = cgus
        .iter()
        .map(|cgu| cgu.items_in_deterministic_order(tcx).into_iter())
        .flatten()
        .collect::<FxHashMap<_, (_, _)>>()
        .into_iter()
        .collect::<Vec<(_, (_, _))>>();

    tcx.sess.time("codegen mono items", || {
        super::predefine_mono_items(tcx, &mut interpret_module, &mono_items);

        let mut cx = crate::CodegenCx::new(
            tcx,
            interpret_module.isa(),
            false,
            Symbol::intern("dummy_cgu_name"),
        );
        let mut cached_context = Context::new();

        for (mono_item, _) in mono_items {
            match mono_item {
                MonoItem::Fn(instance) => {
                    tcx.prof.generic_activity("codegen and compile fn").run(|| {
                        let _inst_guard = crate::PrintOnPanic(|| {
                            format!("{:?} {}", instance, tcx.symbol_name(instance).name)
                        });

                        let cached_func =
                            std::mem::replace(&mut cached_context.func, Function::new());
                        let codegened_func = crate::base::codegen_fn(
                            tcx,
                            &mut cx,
                            cached_func,
                            &mut interpret_module,
                            instance,
                        );

                        crate::base::compile_fn(
                            &mut cx,
                            &mut cached_context,
                            &mut interpret_module,
                            codegened_func,
                        );
                    });
                }
                MonoItem::Static(def_id) => {
                    crate::constant::codegen_static(tcx, &mut interpret_module, def_id);
                }
                MonoItem::GlobalAsm(item_id) => {
                    let item = tcx.hir().item(item_id);
                    tcx.sess.span_fatal(item.span, "Global asm is not supported in interpret mode");
                }
            }
        }

        if !cx.global_asm.is_empty() {
            tcx.sess.fatal("Inline asm is not supported in interpret mode");
        }
    });

    tcx.sess.abort_if_errors();

    println!(
        "Rustc codegen cranelift will JIT run the executable, because -Cllvm-args=mode=interpret was passed"
    );

    let mut data_object_addrs = BTreeMap::new();
    for (data_id, data_object) in &interpret_module.inner.data_objects {
        match &data_object.init {
            cranelift_module::Init::Uninitialized | cranelift_module::Init::Zeros { .. } => todo!(),
            cranelift_module::Init::Bytes { contents } => {
                data_object_addrs.insert(*data_id, contents.as_ptr() as u64);
            }
        }
    }

    for (data_id, data_object) in &interpret_module.inner.data_objects {
        for reloc in
            data_object.all_relocs(Reloc::Abs8 /* FIXME use correct size */).collect::<Vec<_>>()
        {
            let reloc_val = (match reloc.name {
                cranelift_module::ModuleExtName::User { namespace, index } => {
                    if namespace == 0 {
                        index as u64 // Use function index as "address" for functions
                    } else if namespace == 1 {
                        data_object_addrs[&DataId::from_u32(index)]
                    } else {
                        unreachable!()
                    }
                }
                cranelift_module::ModuleExtName::LibCall(_) => todo!(),
                cranelift_module::ModuleExtName::KnownSymbol(_) => todo!(),
            } as i64
                + reloc.addend) as u64;
            match reloc.kind {
                Reloc::Abs8 => unsafe {
                    *(data_object_addrs[data_id] as *mut u64) = reloc_val;
                },
                _ => unreachable!(),
            }
        }
    }

    let mut interpreter =
        Interpreter::new(InterpreterState { module: &interpret_module, stack: vec![] });

    match interpreter.call_by_name("main", &[DataValue::U32(0), DataValue::U64(0)]) {
        Ok(call_res) => {
            println!("{:?}", call_res);
        }
        Err(err) => match err {
            InterpreterError::StepError(step_err) => match step_err {
                cranelift_interpreter::step::StepError::UnknownFunction(func_ref) => {
                    let func_id = FuncId::from_u32(
                        match interpreter.state.get_current_function().dfg.ext_funcs[func_ref].name
                        {
                            cranelift_codegen::ir::ExternalName::User(user) => {
                                interpreter.state.get_current_function().params.user_named_funcs
                                    [user]
                                    .index
                            }
                            cranelift_codegen::ir::ExternalName::TestCase(_) => todo!(),
                            cranelift_codegen::ir::ExternalName::LibCall(_) => todo!(),
                            cranelift_codegen::ir::ExternalName::KnownSymbol(_) => todo!(),
                        },
                    );
                    println!(
                        "Imported function {:#?}",
                        interpreter.state.module.declarations().get_function_decl(func_id)
                    );
                }
                step_err => println!("STEP ERROR: {step_err:?}"),
            },
            err => println!("ERROR: {err:?}"),
        },
    }

    std::process::exit(1);
}

struct InterpreterState<'a> {
    module: &'a super::lto::SerializeModule,
    stack: Vec<(Frame<'a>, *mut ())>,
}

impl<'a> InterpreterState<'a> {
    fn current_frame_mut(&mut self) -> &mut Frame<'a> {
        &mut self.stack.last_mut().unwrap().0
    }

    fn current_frame(&self) -> &Frame<'a> {
        &self.stack.last().unwrap().0
    }
}

impl<'a> State<'a, DataValue> for InterpreterState<'a> {
    fn get_function(&self, func_ref: FuncRef) -> Option<InterpreterFunctionRef<'a, DataValue>> {
        let func_id =
            FuncId::from_u32(match self.get_current_function().dfg.ext_funcs[func_ref].name {
                cranelift_codegen::ir::ExternalName::User(user) => {
                    self.get_current_function().params.user_named_funcs[user].index
                }
                cranelift_codegen::ir::ExternalName::TestCase(_) => todo!(),
                cranelift_codegen::ir::ExternalName::LibCall(_) => todo!(),
                cranelift_codegen::ir::ExternalName::KnownSymbol(_) => todo!(),
            });
        println!(
            "Get function {}",
            self.module.declarations().get_function_decl(func_id).linkage_name(func_id)
        );
        match self.module.inner.functions.get(&func_id) {
            Some(func) => Some(InterpreterFunctionRef::Function(func)),
            None => {
                match &*self.module.declarations().get_function_decl(func_id).linkage_name(func_id)
                {
                    "puts" => Some(InterpreterFunctionRef::Emulated(
                        Box::new(|args| {
                            todo!("{args:?}");
                            Ok(smallvec::smallvec![])
                        }),
                        self.module.declarations().get_function_decl(func_id).signature.clone(),
                    )),
                    name => unimplemented!("{name}"),
                }
            }
        }
    }

    fn get_current_function(&self) -> &'a Function {
        self.stack.last().unwrap().0.function()
    }

    fn get_libcall_handler(&self) -> cranelift_interpreter::interpreter::LibCallHandler<DataValue> {
        |libcall, args| todo!("{libcall:?} {args:?}")
    }

    fn push_frame(&mut self, function: &'a Function) {
        self.stack.push((
            Frame::new(function),
            Box::into_raw(
                vec![
                    0;
                    function.sized_stack_slots.values().map(|slot| slot.size).sum::<u32>() as usize
                ]
                .into_boxed_slice(),
            ) as *mut (),
        ));
    }

    fn pop_frame(&mut self) {
        // FIXME free stack
        self.stack.pop().unwrap();
    }

    fn get_value(&self, name: Value) -> Option<DataValue> {
        Some(self.current_frame().get(name).clone())
    }

    fn set_value(&mut self, name: Value, value: DataValue) -> Option<DataValue> {
        self.current_frame_mut().set(name, value)
    }

    fn stack_address(
        &self,
        size: cranelift_interpreter::address::AddressSize,
        slot: StackSlot,
        offset: u64,
    ) -> Result<cranelift_interpreter::address::Address, cranelift_interpreter::state::MemoryError>
    {
        let stack_slots = &self.get_current_function().sized_stack_slots;

        // Calculate the offset from the current frame to the requested stack slot
        let slot_offset: u64 =
            stack_slots.keys().filter(|k| k < &slot).map(|k| stack_slots[k].size as u64).sum();

        let final_offset = slot_offset + offset;

        Ok(Address::from_parts(size, AddressRegion::Stack, 0, unsafe {
            self.stack.last().unwrap().1.add(final_offset as usize) as u64
        })
        .unwrap())
    }

    fn checked_load(
        &self,
        address: cranelift_interpreter::address::Address,
        ty: Type,
        mem_flags: MemFlags,
    ) -> Result<DataValue, cranelift_interpreter::state::MemoryError> {
        println!("{:#016x}", address.offset);
        unsafe {
            Ok(match ty.bytes() {
                1 => DataValue::read_from_slice_ne(&*(address.offset as *mut [u8; 1]), ty),
                2 => DataValue::read_from_slice_ne(&*(address.offset as *mut [u8; 2]), ty),
                4 => DataValue::read_from_slice_ne(&*(address.offset as *mut [u8; 4]), ty),
                8 => DataValue::read_from_slice_ne(&*(address.offset as *mut [u8; 8]), ty),
                16 => DataValue::read_from_slice_ne(&*(address.offset as *mut [u8; 16]), ty),
                _ => unreachable!(),
            })
        }
    }

    fn checked_store(
        &mut self,
        address: cranelift_interpreter::address::Address,
        v: DataValue,
        mem_flags: MemFlags,
    ) -> Result<(), cranelift_interpreter::state::MemoryError> {
        unsafe {
            match v {
                DataValue::I8(val) => *(address.offset as *mut i8) = val,
                DataValue::I16(val) => *(address.offset as *mut i16) = val,
                DataValue::I32(val) => *(address.offset as *mut i32) = val,
                DataValue::I64(val) => *(address.offset as *mut i64) = val,
                DataValue::I128(val) => *(address.offset as *mut i128) = val,
                DataValue::U8(val) => *(address.offset as *mut u8) = val,
                DataValue::U16(val) => *(address.offset as *mut u16) = val,
                DataValue::U32(val) => *(address.offset as *mut u32) = val,
                DataValue::U64(val) => *(address.offset as *mut u64) = val,
                DataValue::U128(val) => *(address.offset as *mut u128) = val,
                DataValue::F32(val) => *(address.offset as *mut f32) = val.as_f32(),
                DataValue::F64(val) => *(address.offset as *mut f64) = val.as_f64(),
                DataValue::V128(val) => todo!(),
                DataValue::V64(val) => todo!(),
            }
        }

        Ok(())
    }

    fn function_address(
        &self,
        size: cranelift_interpreter::address::AddressSize,
        name: &cranelift_codegen::ir::ExternalName,
    ) -> Result<cranelift_interpreter::address::Address, cranelift_interpreter::state::MemoryError>
    {
        todo!()
    }

    fn get_function_from_address(
        &self,
        address: cranelift_interpreter::address::Address,
    ) -> Option<cranelift_interpreter::state::InterpreterFunctionRef<'a, DataValue>> {
        todo!()
    }

    fn resolve_global_value(
        &self,
        gv: cranelift_codegen::ir::GlobalValue,
    ) -> Result<DataValue, cranelift_interpreter::state::MemoryError> {
        match &self.get_current_function().global_values[gv] {
            cranelift_codegen::ir::GlobalValueData::Symbol { name, offset, colocated: _, tls } => {
                assert!(!tls);
                let data_object = &self.module.inner.data_objects[&DataId::from_u32(match name {
                    cranelift_codegen::ir::ExternalName::User(user) => {
                        self.get_current_function().params.user_named_funcs[*user].index
                    }
                    cranelift_codegen::ir::ExternalName::TestCase(_) => todo!(),
                    cranelift_codegen::ir::ExternalName::LibCall(_) => todo!(),
                    cranelift_codegen::ir::ExternalName::KnownSymbol(_) => todo!(),
                })];
                Ok(DataValue::I64(match &data_object.init {
                    cranelift_module::Init::Uninitialized
                    | cranelift_module::Init::Zeros { .. } => unreachable!(),
                    cranelift_module::Init::Bytes { contents } => {
                        dbg!(contents.as_ptr() as i64 + offset.bits())
                    }
                }))
            }
            _ => unreachable!(),
        }
    }

    fn get_pinned_reg(&self) -> DataValue {
        todo!()
    }

    fn set_pinned_reg(&mut self, v: DataValue) {
        todo!()
    }
}

// Adopted from cranelift_interpreter::interpreter::Interpreter

/// The Cranelift interpreter; this contains some high-level functions to control the interpreter's
/// flow. The interpreter state is defined separately (see [InterpreterState]) as the execution
/// semantics for each Cranelift instruction (see [step]).
struct Interpreter<'a> {
    state: InterpreterState<'a>,
}

impl<'a> Interpreter<'a> {
    fn new(state: InterpreterState<'a>) -> Self {
        Self { state }
    }

    /// Call a function by name; this is a helpful proxy for [Interpreter::call_by_index].
    fn call_by_name(
        &mut self,
        func_name: &str,
        arguments: &[DataValue],
    ) -> Result<ControlFlow<'a, DataValue>, InterpreterError> {
        let func_id = match self.state.module.declarations().get_name(func_name).unwrap() {
            cranelift_module::FuncOrDataId::Func(func_id) => func_id,
            cranelift_module::FuncOrDataId::Data(_) => panic!(),
        };

        let func = &self.state.module.inner.functions[&func_id];

        self.call(func, arguments)
    }

    /// Interpret a call to a [Function] given its [DataValue] arguments.
    fn call(
        &mut self,
        function: &'a Function,
        arguments: &[DataValue],
    ) -> Result<ControlFlow<'a, DataValue>, InterpreterError> {
        let first_block = function.layout.blocks().next().expect("to have a first block");
        let parameters = function.dfg.block_params(first_block);
        self.state.push_frame(function);
        self.state.current_frame_mut().set_all(parameters, arguments.to_vec());

        self.block(first_block)
    }

    /// Interpret a [Block] in a [Function]. This drives the interpretation over sequences of
    /// instructions, which may continue in other blocks, until the function returns.
    fn block(&mut self, block: Block) -> Result<ControlFlow<'a, DataValue>, InterpreterError> {
        let function = self.state.current_frame_mut().function();
        let layout = &function.layout;
        let mut maybe_inst = layout.first_inst(block);
        while let Some(inst) = maybe_inst {
            let inst_context = DfgInstructionContext::new(inst, &function.dfg);
            match step(&mut self.state, inst_context)? {
                ControlFlow::Assign(values) => {
                    self.state
                        .current_frame_mut()
                        .set_all(function.dfg.inst_results(inst), values.to_vec());
                    maybe_inst = layout.next_inst(inst)
                }
                ControlFlow::Continue => maybe_inst = layout.next_inst(inst),
                ControlFlow::ContinueAt(block, block_arguments) => {
                    self.state
                        .current_frame_mut()
                        .set_all(function.dfg.block_params(block), block_arguments.to_vec());
                    maybe_inst = layout.first_inst(block)
                }
                ControlFlow::Call(called_function, arguments) => {
                    let returned_arguments =
                        self.call(called_function, &arguments)?.unwrap_return();
                    self.state
                        .current_frame_mut()
                        .set_all(function.dfg.inst_results(inst), returned_arguments);
                    maybe_inst = layout.next_inst(inst)
                }
                ControlFlow::ReturnCall(callee, args) => {
                    self.state.pop_frame();
                    let rets = self.call(callee, &args)?.unwrap_return();
                    return Ok(ControlFlow::Return(rets.into()));
                }
                ControlFlow::Return(returned_values) => {
                    self.state.pop_frame();
                    return Ok(ControlFlow::Return(returned_values));
                }
                ControlFlow::Trap(trap) => return Ok(ControlFlow::Trap(trap)),
            }
        }
        Err(InterpreterError::Unreachable)
    }
}
