use module::{Module, Function, Type, Export, Table};
use prelude::{Box, Vec, String};
use prelude::str;
use opcode::Opcode;
use int_ops;
use value::Value;
use prelude;
use fp_ops;

const PAGE_SIZE: usize = 65536;

#[derive(Debug)]
pub enum ExecuteError {
    Custom(String),
    OperandStackUnderflow,
    NotImplemented,
    TableIndexOutOfBound,
    TypeIdxIndexOufOfBound,
    FunctionIndexOutOfBound,
    OpcodeIndexOutOfBound,
    FrameIndexOutOfBound,
    LocalIndexOutOfBound(usize),
    GlobalIndexOutOfBound,
    UnreachableExecuted,
    AddrOutOfBound(u32, u32),
    TypeMismatch,
    ValueTypeMismatch,
    ReturnTypeMismatch,
    UndefinedTableEntry,
    FunctionNotFound,
    ExportEntryNotFound,
    InvalidMemoryOperation,
    FloatingPointException,
    IndirectCallTypeMismatch(usize, Type, Type) // (fn_id, expected, actual)
}

impl prelude::fmt::Display for ExecuteError {
    fn fmt(&self, f: &mut prelude::fmt::Formatter) -> Result<(), prelude::fmt::Error> {
        <Self as prelude::fmt::Debug>::fmt(self, f)
    }
}

pub type ExecuteResult<T> = Result<T, ExecuteError>;

pub struct VirtualMachine<'a> {
    module: &'a Module,
    rt: RuntimeInfo,
    frames: Vec<Frame>,
    native_functions: Vec<NativeFunctionInfo>
}

pub struct NativeFunctionInfo {
    f: NativeFunction,
    typeidx: usize
}

pub type NativeEntry = Box<Fn(&mut RuntimeInfo, &[Value]) -> ExecuteResult<Option<Value>> + 'static>;

pub enum NativeFunction {
    Uninitialized(String, String), // (module, field)
    Ready(NativeEntry)
}

pub trait NativeResolver: 'static {
    fn resolve(&self, module: &str, field: &str) -> Option<NativeEntry>;
}

#[derive(Copy, Clone, Debug)]
pub enum Mutable {
    Const,
    Mut
}

pub struct RuntimeInfo {
    pub(crate) debug_print_hook: Option<fn(s: &str)>,
    pub(crate) mem: Memory,
    pub(crate) globals: Vec<Value>,
    pub(crate) resolver: Box<NativeResolver>
}

pub struct RuntimeConfig {
    pub mem_default_size_pages: usize,
    pub mem_max_size_pages: Option<usize>,
    pub resolver: Box<NativeResolver>
}

impl NativeFunction {
    pub fn invoke(&mut self, rt: &mut RuntimeInfo, args: &[Value]) -> ExecuteResult<Option<Value>> {
        match *self {
            NativeFunction::Uninitialized(ref m, ref f) => {
                let target = match rt.resolver.resolve(m.as_str(), f.as_str()) {
                    Some(v) => v,
                    None => {
                        match NativeFunction::builtin_resolve(rt, m.as_str(), f.as_str()) {
                            Some(v) => v,
                            None => return Err(ExecuteError::FunctionNotFound)
                        }
                    }
                };
                *self = NativeFunction::Ready(target);
            },
            _ => {}
        }

        if let NativeFunction::Ready(ref f) = *self {
            f(rt, args)
        } else {
            Err(ExecuteError::UnreachableExecuted)
        }
    }

    fn builtin_resolve(rt: &RuntimeInfo, module: &str, field: &str) -> Option<NativeEntry> {
        if module != "env" {
            return None;
        }

        let debug_print = rt.debug_print_hook;

        match field {
            "__wcore_print" => Some(Box::new(move |rt, args| {
                if args.len() != 2 {
                    return Err(ExecuteError::TypeMismatch);
                }
                let ptr = args[0].get_i32()? as usize;
                let len = args[1].get_i32()? as usize;
                if ptr >= rt.mem.data.len() || ptr + len < ptr || ptr + len > rt.mem.data.len() {
                    return Err(ExecuteError::AddrOutOfBound(ptr as u32, len as u32));
                }
                let text = match str::from_utf8(&rt.mem.data[ptr..ptr + len]) {
                    Ok(v) => v,
                    Err(_) => return Err(ExecuteError::Custom("Invalid UTF-8".to_string()))
                };
                if let Some(f) = debug_print {
                    f(text);
                }
                Ok(None)
            })),
            _ => None
        }
    }
}

impl RuntimeInfo {
    pub fn new(config: RuntimeConfig) -> RuntimeInfo {
        RuntimeInfo {
            debug_print_hook: None,
            mem: Memory::new(
                config.mem_default_size_pages * PAGE_SIZE,
                config.mem_max_size_pages.map(|v| v * PAGE_SIZE)
            ),
            globals: Vec::new(),
            resolver: config.resolver
        }
    }

    pub fn debug_print(&self, s: &str) {
        if let Some(hook) = self.debug_print_hook {
            hook(s);
        }
    }

    pub fn get_memory(&self) -> &[u8] {
        self.mem.data.as_slice()
    }

    pub fn get_memory_mut(&mut self) -> &mut Vec<u8> {
        &mut self.mem.data
    }
}

pub struct Memory {
    pub(crate) data: Vec<u8>,
    max_size: Option<usize>
}

impl Memory {
    pub fn new(default_size: usize, max_size: Option<usize>) -> Memory {
        Memory {
            data: vec![0; default_size],
            max_size: max_size
        }
    }

    pub fn current_size(&self) -> Value {
        Value::I32((self.data.len() / PAGE_SIZE) as i32)
    }

    pub fn grow(&mut self, n_pages: i32) -> Value {
        if n_pages <= 0 {
            return Value::I32(-1);
        }
        let n_pages = n_pages as usize;

        // FIXME: Hardcoded limit for now (prevent overflow etc.)
        if n_pages > 16384 {
            return Value::I32(-1);
        }

        let len_inc = n_pages * PAGE_SIZE;
        let after_inc = self.data.len() + len_inc;

        // Overflow?
        if after_inc <= self.data.len() {
            return Value::I32(-1);
        }

        // Check for the limit
        if let Some(limit) = self.max_size {
            if after_inc > limit {
                return Value::I32(-1);
            }
        }

        let prev_size = self.data.len() / PAGE_SIZE;

        self.data.resize(after_inc, 0);
        //panic!("After inc: {}", after_inc);

        Value::I32(prev_size as i32)
    }
}

#[derive(Clone, Debug)]
pub struct Backtrace {
    pub frames: Vec<BtFrame>
}

#[derive(Clone, Debug)]
pub struct BtFrame {
    pub name: Option<String>
}

pub struct Frame {
    func_id: usize,
    ip: Option<usize>,
    operands: Vec<Value>,
    locals: Vec<Value>
}

impl Frame {
    pub fn setup(func_id: usize, func: &Function) -> Frame {
        Frame {
            func_id: func_id,
            ip: None,
            operands: Vec::new(),
            locals: vec![Value::default(); func.locals.len()]
        }
    }

    pub fn setup_no_locals(func_id: usize) -> Frame {
        Frame {
            func_id: func_id,
            ip: None,
            operands: Vec::new(),
            locals: Vec::new()
        }
    }

    pub fn top_operand(&self) -> ExecuteResult<Value> {
        match self.operands.last() {
            Some(v) => Ok(*v),
            None => Err(ExecuteError::OperandStackUnderflow)
        }
    }

    pub fn pop_operand(&mut self) -> ExecuteResult<Value> {
        match self.operands.pop() {
            Some(v) => Ok(v),
            None => Err(ExecuteError::OperandStackUnderflow)
        }
    }

    pub fn push_operand(&mut self, operand: Value) {
        self.operands.push(operand);
    }

    pub fn set_local(&mut self, idx: u32, val: Value) -> ExecuteResult<()> {
        let idx = idx as usize;

        if idx >= self.locals.len() {
            Err(ExecuteError::LocalIndexOutOfBound(idx))
        } else {
            self.locals[idx] = val;
            Ok(())
        }
    }

    pub fn get_local(&mut self, idx: u32) -> ExecuteResult<Value> {
        let idx = idx as usize;

        if idx >= self.locals.len() {
            Err(ExecuteError::LocalIndexOutOfBound(idx))
        } else {
            Ok(self.locals[idx])
        }
    }
}

impl<'a> VirtualMachine<'a> {
    pub fn new(module: &'a Module, rt_config: RuntimeConfig) -> ExecuteResult<VirtualMachine<'a>> {
        let mut vm = VirtualMachine {
            module: module,
            rt: RuntimeInfo::new(rt_config),
            frames: Vec::new(),
            native_functions: Vec::new()
        };

        for ds in &module.data_segments {
            let offset = ds.offset as usize;
            if offset >= vm.rt.mem.data.len() || offset + ds.data.len() > vm.rt.mem.data.len() {
                return Err(ExecuteError::AddrOutOfBound(offset as u32, ds.data.len() as u32));
            }
            for i in 0..ds.data.len() {
                vm.rt.mem.data[offset + i] = ds.data[i];
            }
        }

        for g in &module.globals {
            vm.rt.globals.push(g.value);
        }

        for n in &module.natives {
            vm.native_functions.push(NativeFunctionInfo {
                f: NativeFunction::Uninitialized(
                    n.module.clone(),
                    n.field.clone()
                ),
                typeidx: n.typeidx as usize
            });
        }

        Ok(vm)
    }

    pub fn get_runtime_info(&self) -> &RuntimeInfo {
        &self.rt
    }

    pub fn get_runtime_info_mut(&mut self) -> &mut RuntimeInfo {
        &mut self.rt
    }

    pub fn lookup_exported_func(&self, name: &str) -> ExecuteResult<usize> {
        match self.module.exports.get(name) {
            Some(v) => match *v {
                Export::Function(id) => Ok(id as usize)
            },
            None => Err(ExecuteError::FunctionNotFound)
        }
    }

    pub fn last_function(&'a self) -> Option<&'a Function> {
        let frame: &Frame = match self.frames.last() {
            Some(v) => v,
            None => return None
        };
        Some(&self.module.functions[frame.func_id])
    }

    pub fn backtrace(&self) -> Backtrace {
        let mut bt_frames: Vec<BtFrame> = Vec::new();
        for f in self.frames.iter().rev() {
            let func_id = f.func_id;
            let func = &self.module.functions[func_id];
            bt_frames.push(BtFrame {
                name: func.name.clone()
            });
        }

        Backtrace {
            frames: bt_frames
        }
    }

    pub fn set_debug_print_hook(&mut self, f: fn(s: &str)) {
        self.rt.debug_print_hook = Some(f);
    }

    fn prep_invoke(
        module: &Module,
        frame: &mut Frame,
        idx: usize
    ) -> ExecuteResult<Frame> {
        if idx >= module.functions.len() {
            return Err(ExecuteError::FunctionIndexOutOfBound);
        }
        let current_func = &module.functions[idx];

        // Now we've switched the current function to the new one.
        // Initialize the new frame now.

        let mut new_frame = Frame::setup_no_locals(idx);

        let ty = if (current_func.typeidx as usize) < module.types.len() {
            &module.types[current_func.typeidx as usize]
        } else {
            return Err(ExecuteError::TypeIdxIndexOufOfBound);
        };

        let n_args = match *ty {
            Type::Func(ref args, _) => args.len(),
            _ => return Err(ExecuteError::TypeMismatch)
        };

        let n_locals = current_func.locals.len();

        // Initialize the new locals.
        new_frame.locals = vec![Value::default(); n_args + n_locals];

        for i in 0..n_args {
            let arg_v = frame.pop_operand()?;
            new_frame.locals[n_args - 1 - i] = arg_v;
        }

        Ok(new_frame)
    }

    pub fn execute(
        &mut self,
        initial_func: usize,
        args: &[Value]
    ) -> ExecuteResult<Option<Value>> {
        let mut current_func: &Function = &self.module.functions[initial_func];
        let initial_stack_depth: usize = self.frames.len();
        let debug_print = self.rt.debug_print_hook;

        // FIXME: Handle initial call gracefully
        {
            if current_func.typeidx as usize >= self.module.types.len() {
                return Err(ExecuteError::TypeIdxIndexOufOfBound);
            }

            let Type::Func(ref initial_func_args_type, _) = self.module.types[current_func.typeidx as usize];
            if args.len() != initial_func_args_type.len() {
                return Err(ExecuteError::TypeMismatch);
            }
            let mut initial_frame = Frame::setup_no_locals(initial_func);
            initial_frame.locals = vec! [ Value::default(); args.len() + current_func.locals.len() ];
            for i in 0..args.len() {
                initial_frame.locals[i] = args[i];
            }
            self.frames.push(initial_frame);
        }

        let mut ip: usize = 0;

        loop {
            let frame: &mut Frame = match self.frames.last_mut() {
                Some(v) => v,
                None => return Err(ExecuteError::FrameIndexOutOfBound)
            };

            // Fetch the current instruction and move to the next one.
            if ip >= current_func.body.opcodes.len() {
                return Err(ExecuteError::OpcodeIndexOutOfBound);
            }
            let op = &current_func.body.opcodes[ip];
            ip += 1;

            /*if let Some(f) = debug_print {
                f(format!("{:?}", op).as_str());
            }*/

            match *op {
                Opcode::Drop => {
                    frame.pop_operand()?;
                },
                Opcode::Select => {
                    let c = frame.pop_operand()?.get_i32()?;
                    let val2 = frame.pop_operand()?;
                    let val1 = frame.pop_operand()?;
                    if c != 0 {
                        frame.push_operand(val1);
                    } else {
                        frame.push_operand(val2);
                    }
                },
                Opcode::Call(idx) => {
                    // "Push" IP so that we can restore it after the call is done.
                    frame.ip = Some(ip);

                    // Reset IP.
                    ip = 0;

                    let new_frame = Self::prep_invoke(&self.module, frame, idx as usize)?;
                    current_func = &self.module.functions[new_frame.func_id];

                    // Push the newly-created frame.
                    self.frames.push(new_frame);
                    
                },
                Opcode::CallIndirect(typeidx) => {
                    let typeidx = typeidx as usize;
                    if self.module.tables.len() == 0 {
                        return Err(ExecuteError::TableIndexOutOfBound);
                    }
                    let table: &Table = &self.module.tables[0];

                    if typeidx >= self.module.types.len() {
                        return Err(ExecuteError::TypeIdxIndexOufOfBound);
                    }
                    let ft_expect = &self.module.types[typeidx];

                    let index = frame.pop_operand()?.get_i32()? as usize;
                    if index >= table.elements.len() {
                        return Err(ExecuteError::TableIndexOutOfBound);
                    }

                    let elem: u32 = if let Some(v) = table.elements[index] {
                        v
                    } else {
                        return Err(ExecuteError::UndefinedTableEntry);
                    };

                    if elem as usize >= self.module.functions.len() {
                        return Err(ExecuteError::FunctionIndexOutOfBound);
                    }

                    let actual_typeidx = self.module.functions[elem as usize].typeidx as usize;
                    if actual_typeidx >= self.module.types.len() {
                        return Err(ExecuteError::TypeIdxIndexOufOfBound);
                    }
                    if self.module.types[actual_typeidx] != *ft_expect {
                        return Err(ExecuteError::IndirectCallTypeMismatch(
                            elem as usize,
                            ft_expect.clone(),
                            self.module.types[actual_typeidx].clone()
                        ));
                    }

                    frame.ip = Some(ip);
                    ip = 0;

                    let new_frame = Self::prep_invoke(&self.module, frame, elem as usize)?;
                    current_func = &self.module.functions[new_frame.func_id];

                    // Push the newly-created frame.
                    self.frames.push(new_frame);
                },
                Opcode::NativeInvoke(id) => {
                    let id = id as usize;

                    if id >= self.native_functions.len() {
                        return Err(ExecuteError::FunctionIndexOutOfBound);
                    }
                    let f: &mut NativeFunctionInfo = &mut self.native_functions[id];

                    if f.typeidx >= self.module.types.len() {
                        return Err(ExecuteError::TypeIdxIndexOufOfBound);
                    }

                    let Type::Func(ref args, ref expected_ret) = self.module.types[f.typeidx];
                    if args.len() > frame.operands.len() {
                        return Err(ExecuteError::OperandStackUnderflow);
                    }
                    let ret = f.f.invoke(&mut self.rt, &frame.operands[frame.operands.len() - args.len()..frame.operands.len()])?;

                    if (expected_ret.len() != 0 && ret.is_none()) || (expected_ret.len() == 0 && ret.is_some()) {
                        return Err(ExecuteError::TypeMismatch);
                    }

                    for _ in 0..args.len() {
                        frame.pop_operand()?;
                    }

                    if let Some(v) = ret {
                        frame.push_operand(v);
                    }
                },
                Opcode::Return => {
                    // Pop the current frame.
                    let mut prev_frame = self.frames.pop().unwrap();

                    let ty = if (current_func.typeidx as usize) < self.module.types.len() {
                        &self.module.types[current_func.typeidx as usize]
                    } else {
                        return Err(ExecuteError::TypeIdxIndexOufOfBound);
                    };

                    let n_rets = match *ty {
                        Type::Func(_ , ref rets) => rets.len(),
                        _ => return Err(ExecuteError::TypeMismatch)
                    };

                    // There should be exactly n_rets operands now.
                    if prev_frame.operands.len() != n_rets {
                        return Err(ExecuteError::ReturnTypeMismatch);
                    }

                    if self.frames.len() < initial_stack_depth {
                        return Err(ExecuteError::Custom("BUG: Invalid frames".into()));
                    }

                    if self.frames.len() == initial_stack_depth {
                        return Ok(if n_rets == 0 {
                            None
                        } else {
                            Some(prev_frame.operands[0])
                        });
                    }

                    // self.frames.len() > initial_stack_depth >= 0 always hold here.

                    // Restore IP.
                    let frame: &mut Frame = self.frames.last_mut().unwrap();
                    ip = frame.ip.take().unwrap();

                    for op in &prev_frame.operands {
                        frame.push_operand(*op);
                    }

                    current_func = &self.module.functions[frame.func_id];
                },
                Opcode::CurrentMemory => {
                    frame.push_operand(self.rt.mem.current_size());
                },
                Opcode::GrowMemory => {
                    let n_pages = frame.pop_operand()?.get_i32()?;
                    frame.push_operand(self.rt.mem.grow(n_pages));
                },
                Opcode::Nop => {},
                Opcode::Jmp(target) => {
                    ip = target as usize;
                },
                Opcode::JmpIf(target) => {
                    let v = frame.pop_operand()?.get_i32()?;
                    if v != 0 {
                        ip = target as usize;
                    }
                },
                Opcode::JmpEither(if_true, if_false) => {
                    let v = frame.pop_operand()?.get_i32()?;
                    if v != 0 {
                        ip = if_true as usize;
                    } else {
                        ip = if_false as usize;
                    }
                },
                Opcode::JmpTable(ref table, otherwise) => {
                    let v = frame.pop_operand()?.get_i32()? as usize;
                    if v < table.len() {
                        ip = table[v] as usize;
                    } else {
                        ip = otherwise as usize;
                    }
                },
                Opcode::SetLocal(idx) => {
                    let v = frame.pop_operand()?;
                    frame.set_local(idx, v)?;
                },
                Opcode::GetLocal(idx) => {
                    let v = frame.get_local(idx)?;
                    frame.push_operand(v);
                },
                Opcode::TeeLocal(idx) => {
                    let v = frame.top_operand()?;
                    frame.set_local(idx, v)?;
                },
                Opcode::GetGlobal(idx) => {
                    let idx = idx as usize;
                    if idx >= self.rt.globals.len() {
                        return Err(ExecuteError::GlobalIndexOutOfBound);
                    }
                    frame.push_operand(self.rt.globals[idx])
                },
                Opcode::SetGlobal(idx) => {
                    let idx = idx as usize;
                    if idx >= self.rt.globals.len() {
                        return Err(ExecuteError::GlobalIndexOutOfBound);
                    }
                    let v = frame.pop_operand()?;
                    self.rt.globals[idx] = v;
                },
                Opcode::Unreachable => {
                    return Err(ExecuteError::UnreachableExecuted);
                },
                Opcode::I32Const(v) => {
                    frame.push_operand(Value::I32(v));
                },
                Opcode::I32Clz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_clz(v.get_i32()?));
                },
                Opcode::I32Ctz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_ctz(v.get_i32()?));
                },
                Opcode::I32Popcnt => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_popcnt(v.get_i32()?));
                },
                Opcode::I32Add => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_add(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Sub => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_sub(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Mul => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_mul(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32DivU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_div_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32DivS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_div_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32RemU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_rem_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32RemS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_rem_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32And => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_and(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Or => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_or(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Xor => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_xor(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Shl => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_shl(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32ShrU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_shr_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32ShrS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_shr_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Rotl => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_rotl(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Rotr => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_rotr(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Eqz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_eqz(v.get_i32()?));
                },
                Opcode::I32Eq => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_eq(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32Ne => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_ne(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32LtU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_lt_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32LtS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_lt_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32LeU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_le_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32LeS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_le_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32GtU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_gt_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32GtS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_gt_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32GeU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_ge_u(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32GeS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_ge_s(c1.get_i32()?, c2.get_i32()?));
                },
                Opcode::I32WrapI64 => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i32_wrap_i64(v.get_i64()?));
                },
                Opcode::I32Load(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i32_load_u(i, m, &mut self.rt.mem, 4)?;
                    frame.push_operand(v);
                },
                Opcode::I32Load8U(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i32_load_u(i, m, &mut self.rt.mem, 1)?;
                    frame.push_operand(v);
                },
                Opcode::I32Load8S(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i32_load_s(i, m, &mut self.rt.mem, 1)?;
                    frame.push_operand(v);
                },
                Opcode::I32Load16U(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i32_load_u(i, m, &mut self.rt.mem, 2)?;
                    frame.push_operand(v);
                },
                Opcode::I32Load16S(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i32_load_s(i, m, &mut self.rt.mem, 2)?;
                    frame.push_operand(v);
                },
                Opcode::I32Store(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i32_store(i, c, m, &mut self.rt.mem, 4)?;
                },
                Opcode::I32Store8(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i32_store(i, c, m, &mut self.rt.mem, 1)?;
                },
                Opcode::I32Store16(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i32_store(i, c, m, &mut self.rt.mem, 2)?;
                },
                Opcode::I64Const(v) => {
                    frame.push_operand(Value::I64(v));
                },
                Opcode::I64Clz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_clz(v.get_i64()?));
                },
                Opcode::I64Ctz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_ctz(v.get_i64()?));
                },
                Opcode::I64Popcnt => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_popcnt(v.get_i64()?));
                },
                Opcode::I64Add => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_add(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Sub => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_sub(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Mul => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_mul(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64DivU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_div_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64DivS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_div_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64RemU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_rem_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64RemS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_rem_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64And => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_and(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Or => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_or(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Xor => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_xor(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Shl => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_shl(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64ShrU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_shr_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64ShrS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_shr_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Rotl => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_rotl(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Rotr => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_rotr(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Eqz => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_eqz(v.get_i64()?));
                },
                Opcode::I64Eq => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_eq(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64Ne => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_ne(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64LtU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_lt_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64LtS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_lt_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64LeU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_le_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64LeS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_le_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64GtU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_gt_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64GtS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_gt_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64GeU => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_ge_u(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64GeS => {
                    let c2 = frame.pop_operand()?;
                    let c1 = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_ge_s(c1.get_i64()?, c2.get_i64()?));
                },
                Opcode::I64ExtendI32U => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_extend_i32_u(v.get_i32()?));
                },
                Opcode::I64ExtendI32S => {
                    let v = frame.pop_operand()?;
                    frame.push_operand(int_ops::i64_extend_i32_s(v.get_i32()?));
                },
                Opcode::I64Load(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_u(i, m, &mut self.rt.mem, 8)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load8U(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_u(i, m, &mut self.rt.mem, 1)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load8S(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_s(i, m, &mut self.rt.mem, 1)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load16U(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_u(i, m, &mut self.rt.mem, 2)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load16S(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_s(i, m, &mut self.rt.mem, 2)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load32U(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_u(i, m, &mut self.rt.mem, 4)?;
                    frame.push_operand(v);
                },
                Opcode::I64Load32S(ref m) => {
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    let v = int_ops::i64_load_s(i, m, &mut self.rt.mem, 4)?;
                    frame.push_operand(v);
                },
                Opcode::I64Store(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i64_store(i, c, m, &mut self.rt.mem, 8)?;
                },
                Opcode::I64Store8(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i64_store(i, c, m, &mut self.rt.mem, 1)?;
                },
                Opcode::I64Store16(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i64_store(i, c, m, &mut self.rt.mem, 2)?;
                },
                Opcode::I64Store32(ref m) => {
                    let c = frame.pop_operand()?;
                    let i = frame.pop_operand()?.get_i32()? as u32;
                    int_ops::i64_store(i, c, m, &mut self.rt.mem, 4)?;
                },
                Opcode::NotImplemented(ref s) => {
                    return Err(ExecuteError::Custom(
                        format!("Not implemented: {}", s)
                    ))
                },
                Opcode::Memcpy => {
                    // Pop in reverse order.
                    // (dest, src, n_bytes)
                    let n_bytes = frame.pop_operand()?.get_i32()? as usize;
                    let src = frame.pop_operand()?.get_i32()? as usize;
                    let dest = frame.pop_operand()?.get_i32()? as usize;
                    let mem = self.rt.mem.data.as_mut_slice();

                    if dest + n_bytes >= mem.len() {
                        return Err(ExecuteError::AddrOutOfBound(dest as u32, n_bytes as u32));
                    }
                    if src + n_bytes >= mem.len() {
                        return Err(ExecuteError::AddrOutOfBound(src as u32, n_bytes as u32));
                    }

                    for i in 0..n_bytes {
                        mem[dest + i] = mem[src + i]; // copy_from_slice ?
                    }
                },
                Opcode::F32Const(v) => {
                    frame.push_operand(Value::F32(fp_ops::i32_reinterpret_f32(v as i32)));
                },
                Opcode::F64Const(v) => {
                    frame.push_operand(Value::F64(fp_ops::i64_reinterpret_f64(v as i64)));
                },
                //_ => return Err(ExecuteError::NotImplemented)
            }
        }
    }
}

impl Module {
    pub fn execute(&self, rt_config: RuntimeConfig, initial_func: usize) -> ExecuteResult<Option<Value>> {
        let mut vm = VirtualMachine::new(self, rt_config)?;
        vm.execute(initial_func, &[])
    }
}
