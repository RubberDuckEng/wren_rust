// analog to wren_vm.c from wren_c.

include!(concat!(env!("OUT_DIR"), "/wren_core_source.rs"));

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::{str, usize};

use crate::compiler::{compile_in_module, wren_is_local_name, FnDebug, InputManager, Ops, Scope};
use crate::core::{init_base_classes, init_fn_and_fiber, register_core_primitives};
use crate::wren::{DebugLevel, WrenConfiguration, WrenForeignMethodFn};

type Result<T, E = VMError> = std::result::Result<T, E>;

pub(crate) const CORE_MODULE_NAME: &str = "#core";

// The maximum number of module-level variables that may be defined at one time.
const MAX_MODULE_VARS: usize = 65536;

// The maximum number of arguments that can be passed to a method. Note that
// this limit may be hardcoded in other places in the VM.
pub(crate) const MAX_PARAMETERS: usize = 16;

// The maximum number of fields a class can have, including inherited fields.
// This is explicit in the bytecode since `CODE_CLASS` and `CODE_SUBCLASS` take
// a single byte for the number of fields. Note that it's 255 and not 256
// because creating a class takes the *number* of fields, not the *highest
// field index*.
const MAX_FIELDS: usize = 255;

// Internal VM Error, wrapped in RuntimeError for API.
#[derive(Debug)]
pub(crate) enum VMError {
    Error(String),
    FiberAbort(Value),
}

impl VMError {
    // Rename to error_str?
    pub(crate) fn from_str(msg: &str) -> VMError {
        VMError::Error(msg.into())
    }

    // Rename to error_string?
    pub(crate) fn from_string(msg: String) -> VMError {
        VMError::Error(msg)
    }

    fn as_try_return_value(&self) -> Value {
        match self {
            VMError::Error(string) => Value::from_str(string),
            VMError::FiberAbort(value) => value.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Value {
    Null,
    Num(f64),
    Boolean(bool),
    String(Rc<String>),
    // Split these off and replace with Object(Handle<dyn Obj>)
    Class(Handle<ObjClass>),
    Range(Handle<ObjRange>),
    Fn(Handle<ObjFn>),
    Closure(Handle<ObjClosure>),
    List(Handle<ObjList>),
    Map(Handle<ObjMap>),
    Fiber(Handle<ObjFiber>),
    Instance(Handle<ObjInstance>),
}

impl Hash for Value {
    // See hashObject in wren_c wren_value.c
    fn hash<H: Hasher>(&self, state: &mut H) {
        fn hash_f64<H: Hasher>(num: f64, state: &mut H) {
            let bits: u64 = unsafe { std::mem::transmute(num) };
            bits.hash(state);
        }
        match self {
            Value::Null => 0.hash(state),
            Value::Num(f64_ref) => hash_f64(*f64_ref, state),
            Value::Boolean(v) => v.hash(state),
            Value::String(v) => v.hash(state),
            Value::Class(v) => v.borrow().name.hash(state),
            Value::Range(v) => {
                hash_f64(v.borrow().from, state);
                hash_f64(v.borrow().to, state);
                // wren_c doesn't hash inclusive, but we could?
            }
            // FIXME: wren_c defines a hash for Fn for internal usage?
            Value::Fn(v) => v.as_ptr().hash(state),
            // FIXME: The rest of these may be wrong?
            Value::Closure(v) => v.as_ptr().hash(state),
            Value::List(v) => v.as_ptr().hash(state),
            Value::Map(v) => v.as_ptr().hash(state),
            Value::Fiber(v) => v.as_ptr().hash(state),
            Value::Instance(v) => v.as_ptr().hash(state),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, rhs: &Value) -> bool {
        // wren_c does memcmp here (e.g. wrenValuesEqual)
        // However the code below seems to pass all the tests.
        match (self, rhs) {
            (Value::Null, Value::Null) => return true,
            (Value::Num(a), Value::Num(b)) => return a == b,
            (Value::Boolean(a), Value::Boolean(b)) => return a == b,
            (Value::Range(a_range), Value::Range(b_range)) => {
                let a = a_range.borrow();
                let b = b_range.borrow();
                return a.from == b.from && a.to == b.to && a.is_inclusive == b.is_inclusive;
            }
            (Value::String(a_string), Value::String(b_string)) => return a_string.eq(&b_string),
            (Value::Class(a), Value::Class(b)) => return a.as_ptr() == b.as_ptr(),
            (Value::Instance(a), Value::Instance(b)) => return a.as_ptr() == b.as_ptr(),
            _ => return false,
        }
    }
}

impl Eq for Value {}

impl core::fmt::Debug for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::Null => write!(f, "Null"),
            Value::Num(n) => write!(f, "{}", n),
            Value::Boolean(b) => write!(f, "{}", b),
            Value::String(s) => write!(f, "\"{}\"", s),
            Value::Class(c) => write!(f, "Class(\"{}\")", c.borrow().name),
            Value::Range(r) => write!(f, "{:?}", r.borrow()),
            Value::List(l) => write!(f, "List(len: {})", l.borrow().elements.len()),
            Value::Map(m) => write!(f, "Map(len: {})", m.borrow().data.len()),
            Value::Fiber(_) => write!(f, "Fiber()"),
            Value::Fn(c) => write!(f, "Fn({})", c.borrow().debug.name),
            Value::Closure(c) => write!(f, "{}", c.borrow().fn_obj.borrow().debug.name),
            Value::Instance(o) => write!(
                f,
                "Instance({})",
                // FIXME: Probably needs a helper on ObjInstance?
                o.borrow().class_obj().borrow().name
            ),
        }
    }
}

impl Value {
    pub(crate) fn from_string(string: String) -> Value {
        Value::String(Rc::new(string))
    }

    pub(crate) fn from_str(string: &str) -> Value {
        Self::from_string(string.into())
    }

    // Make num casts explicit and easily searchable:
    pub(crate) fn from_usize(num: usize) -> Value {
        Self::Num(num as f64)
    }
    pub(crate) fn from_u8(num: u8) -> Value {
        Self::Num(num as f64)
    }
    pub(crate) fn from_u32(num: u32) -> Value {
        Self::Num(num as f64)
    }

    // Different from "is truthy" used by "as_bool" in wren_c
    // Unclear if this is only ever called on a known-bool?
    pub(crate) fn equals_true(&self) -> bool {
        match self {
            Value::Boolean(b) => *b == true,
            _ => false,
        }
    }

    // In Wren false and null are false, everything else is true.
    fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Boolean(b) => *b,
            _ => true,
        }
    }

    fn is_falsey(&self) -> bool {
        !self.is_truthy()
    }

    pub(crate) fn is_null(&self) -> bool {
        match self {
            Value::Null => true,
            _ => false,
        }
    }
    pub(crate) fn is_num(&self) -> bool {
        match self {
            Value::Num(_) => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Module {
    pub name: String,

    // Should this just be a map?  wren_utils.h suggests so?
    variables: Vec<Value>,
    variable_names: Vec<String>,
}

// FIXME: Will turn on in a separate change.
#[allow(dead_code)]
pub(crate) enum ModuleLimitError {
    TooManyVariables,
}

impl Module {
    pub(crate) fn lookup_symbol(&self, name: &str) -> Option<u16> {
        self.variable_names
            .iter()
            .position(|e| e.eq(name))
            .map(|s| s as u16)
    }

    pub(crate) fn variable_count(&self) -> usize {
        self.variables.len()
    }

    pub(crate) fn check_for_undefined_variables<F, E>(
        &self,
        since_index: usize,
        f: F,
    ) -> Result<(), E>
    where
        F: Fn(&str, usize) -> Result<(), E>,
    {
        for i in since_index..self.variables.len() {
            let value = &self.variables[i];
            let name = &self.variable_names[i];
            if let Some(line_number) = value.try_into_num() {
                f(name, line_number as usize)?;
            }
        }
        Ok(())
    }

    pub(crate) fn replace_implicit_definition(&mut self, symbol: u16, value: Value) {
        let index = symbol as usize;
        assert!(self.variables[index].is_num());
        self.variables[index] = value;
    }

    pub(crate) fn define_variable(
        &mut self,
        name: &str,
        value: Value,
    ) -> Result<u16, ModuleLimitError> {
        if self.variables.len() == MAX_MODULE_VARS {
            Err(ModuleLimitError::TooManyVariables)
        } else {
            self.variable_names.push(name.into());
            self.variables.push(value);
            Ok((self.variable_names.len() - 1) as u16)
        }
    }

    pub(crate) fn variable_by_name(&self, name: &str) -> Option<Value> {
        if let Some(index) = self.lookup_symbol(name) {
            Some(self.variables[index as usize].clone())
        } else {
            None
        }
    }

    pub(crate) fn expect_class(&self, name: &str) -> Handle<ObjClass> {
        let symbol = self
            .lookup_symbol(name)
            .expect(&format!("failed to load {}", name));
        self.variables[symbol as usize]
            .try_into_class()
            .expect(&format!("failed to load {}", name))
    }
}

#[derive(Debug)]
pub struct FrameInfo {
    pub module: String,
    pub line: usize,
    pub fn_name: String,
}

#[derive(Debug)]
pub struct StackTrace {
    pub frames: Vec<FrameInfo>,
}

#[derive(Debug)]
pub struct RuntimeError {
    pub msg: String,
    pub stack_trace: StackTrace,
}

impl RuntimeError {
    fn from_error_value(value: Value, stack_trace: StackTrace) -> RuntimeError {
        let maybe_msg = value.try_into_string();
        RuntimeError {
            // [error object] matches wren_c wrenDebugPrintStackTrace
            msg: maybe_msg.unwrap_or("[error object]".to_string()),
            stack_trace: stack_trace,
        }
    }
}

macro_rules! try_into {
    ($func:ident,  $value_type:ident, $return_type:ident) => {
        pub fn $func(&self) -> Option<Handle<$return_type>> {
            match self {
                Value::$value_type(value) => Some(value.clone()),
                _ => None,
            }
        }
    };
}

impl Value {
    pub fn try_into_num(&self) -> Option<f64> {
        match self {
            Value::Num(value) => Some(*value),
            _ => None,
        }
    }

    pub fn try_into_string(&self) -> Option<String> {
        match self {
            Value::String(string) => Some(string.as_ref().clone()),
            _ => None,
        }
    }

    try_into!(try_into_range, Range, ObjRange);
    try_into!(try_into_map, Map, ObjMap);
    try_into!(try_into_class, Class, ObjClass);
    try_into!(try_into_list, List, ObjList);
    try_into!(try_into_fn, Fn, ObjFn);
    try_into!(try_into_closure, Closure, ObjClosure);
    try_into!(try_into_instance, Instance, ObjInstance);
    try_into!(try_into_fiber, Fiber, ObjFiber);
}

#[derive(Debug, Default)]
pub struct SymbolTable {
    names: Vec<String>,
}

impl SymbolTable {
    // This is called methodSymbol or wrenSymbolTableEnsure in wren_c
    pub fn ensure_symbol(&mut self, name: &str) -> usize {
        if let Some(index) = self.symbol_for_name(name) {
            return index;
        }

        // New symbol, so add it.
        self.names.push(name.into());
        self.names.len() - 1
    }

    fn symbol_for_name(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n.eq(name))
    }

    pub fn name_for_symbol(&self, symbol: usize) -> String {
        match self.names.get(symbol) {
            None => "<not found>".into(),
            Some(name) => name.clone(),
        }
    }

    pub fn count(&self) -> usize {
        self.names.len()
    }
}

pub struct CallFrame {
    // Program counter (offset into current code block)
    pc: usize,
    // The closure being executed.
    closure: Handle<ObjClosure>,
    stack: Vec<Value>,
}

impl CallFrame {
    fn new_root(closure: Handle<ObjClosure>) -> CallFrame {
        // The root frame (for an import or interpret)
        // always has the closure at the bottom of the stack.
        CallFrame {
            pc: 0,
            closure: closure.clone(),
            stack: vec![Value::Closure(closure)],
        }
    }
}

impl core::fmt::Debug for CallFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "stack: (len {}, top {:?}), ",
            self.stack.len(),
            self.stack.last()
        )
    }
}

// FIXME: This should express all states instead of "other".
// Tracks how this fiber has been invoked, aside from the ways that can be
// detected from the state of other fields in the fiber.
pub enum FiberRunSource {
    // The fiber is being run from another fiber using a call to `try()`.
    Try,

    // The fiber was directly invoked by `runInterpreter()`. This means it's the
    // initial fiber used by a call to `wrenCall()` or `wrenInterpret()`.
    Root,
    // The fiber is invoked some other way. If [caller] is `NULL` then the fiber
    // was invoked using `call()`. If [numFrames] is zero, then the fiber has
    // finished running and is done. If [numFrames] is one and that frame's `ip`
    // points to the first byte of code, the fiber has not been started yet.
    Other,
}
pub struct ObjFiber {
    class_obj: Handle<ObjClass>,
    pub(crate) error: Value,
    pub(crate) caller: Option<Handle<ObjFiber>>,
    // We can't grab at the call_stack to check if empty
    // while it might be held mutably for execution, so we cache
    // the "completed_normally" bool here.
    // FIXME: This is probably better as an enum?
    pub(crate) completed_normally_cache: bool,
    run_source: FiberRunSource,
    // Hels in a RefCell so others can interact with the rest of
    // ObjFiber (to ask class, etc.) while the stack is  held mutably
    // for the executing fiber.
    call_stack: RefCell<Vec<CallFrame>>,
}

impl ObjFiber {
    // pub fn just_started(&self) -> bool {
    //     self.call_stack.len() == 1 && self.call_stack[0].pc == 0
    // }

    pub fn is_root(&self) -> bool {
        match self.run_source {
            FiberRunSource::Root => true,
            _ => false,
        }
    }

    pub fn is_try(&self) -> bool {
        match self.run_source {
            FiberRunSource::Try => true,
            _ => false,
        }
    }

    fn return_from_fiber_take_caller(&mut self) -> Option<Handle<ObjFiber>> {
        let caller = self.caller.take();
        self.completed_normally_cache = self.call_stack.borrow().is_empty();
        caller
    }

    fn push_value_to_top_of_stack(&mut self, arg: Value) {
        // Push the argument to the fiber call, try or transfer
        // or the return value from a yield or transfer
        // onto the top-most frame of the callstack.
        self.call_stack.borrow_mut().last_mut().unwrap().push(arg);
    }

    pub(crate) fn error(&self) -> Value {
        self.error.clone()
    }

    pub fn has_error(&self) -> bool {
        !self.error.is_null()
    }

    // pub fn takes_one_argument(&self) -> bool {
    //     self.call_stack[0].closure.borrow().fn_obj.borrow().arity == 1
    // }
}

impl Obj for ObjFiber {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

impl ObjFiber {
    pub(crate) fn new(
        vm: &WrenVM,
        closure: Handle<ObjClosure>,
        run_source: FiberRunSource,
    ) -> ObjFiber {
        ObjFiber {
            class_obj: vm.fiber_class.as_ref().unwrap().clone(),
            error: Value::Null,
            caller: None,
            run_source: run_source,
            completed_normally_cache: false,
            call_stack: RefCell::new(vec![CallFrame::new_root(closure)]),
        }
    }
}

pub(crate) fn wren_new_fiber(vm: &WrenVM, closure: Handle<ObjClosure>) -> Handle<ObjFiber> {
    new_handle(ObjFiber::new(vm, closure, FiberRunSource::Other))
}

impl core::fmt::Debug for ObjFiber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let stack = self.call_stack.borrow();
        write!(
            f,
            "call_stack: (len {}, top {:?}), ",
            stack.len(),
            stack.last()
        )
    }
}

pub struct WrenVM {
    // Current executing Fiber (should eventually be a list?)
    pub(crate) fiber: Option<Handle<ObjFiber>>,
    // Single global symbol table for all method names (matches wren_c)
    // Separate Struct to allow easier passing to register_primitive
    pub methods: SymbolTable,
    modules: HashMap<String, Handle<Module>>,

    // The Core module is created first (empty)
    pub(crate) core_module: Option<Handle<Module>>,
    // Then Class
    pub(crate) class_class: Option<Handle<ObjClass>>,
    // Then Fn and Fiber (which are used by wren_core.wren)
    pub(crate) fn_class: Option<Handle<ObjClass>>,
    pub(crate) fiber_class: Option<Handle<ObjClass>>,
    // Finally the rest of wren_core.wren
    pub(crate) core: Option<CoreClasses>,
    pub(crate) start_time: std::time::Instant,
    pub(crate) last_imported_module: Option<Handle<Module>>,
    pub(crate) config: WrenConfiguration,

    // Args for the foreign function being called.
    // wren_c calls this apiStack
    foreign_args: Option<Vec<Value>>,
}

pub trait Clear {
    fn clear(&mut self);
}

impl Clear for Module {
    fn clear(&mut self) {
        self.variables.clear();
        self.variable_names.clear();
    }
}

impl Clear for ObjFiber {
    fn clear(&mut self) {
        self.call_stack.borrow_mut().clear();
    }
}

impl Clear for ObjClass {
    fn clear(&mut self) {
        clear_class(self.class.take());
        clear_class(self.superclass.take());
        self.methods.clear();
    }
}

fn clear_class(maybe_class: Option<Handle<ObjClass>>) {
    if let Some(class) = maybe_class {
        class.take().clear();
    }
}

impl Clear for ObjInstance {
    fn clear(&mut self) {
        self.class_obj.borrow_mut().clear();
        self.fields.clear();
    }
}

impl Clear for ObjList {
    fn clear(&mut self) {
        self.elements.clear();
    }
}

impl Clear for ObjMap {
    fn clear(&mut self) {
        self.data.clear();
    }
}

impl Clear for ObjRange {
    fn clear(&mut self) {
        // No object references held.
    }
}

impl Clear for ObjFn {
    fn clear(&mut self) {
        self.class_obj.borrow_mut().clear();
        self.constants.clear();
        self.code.clear();
        self.module.borrow_mut().clear();
    }
}

impl Clear for ObjClosure {
    fn clear(&mut self) {
        self.class_obj.borrow_mut().clear();
        self.fn_obj.borrow_mut().clear();
    }
}

fn clear_maybe_module(maybe_module: Option<Handle<Module>>) {
    if let Some(module) = maybe_module {
        module.borrow_mut().clear();
    }
}

impl Drop for WrenVM {
    fn drop(&mut self) {
        // Tear down all fibers, including all stacks.
        // Eventually code will be able to hold onto fibers.
        if let Some(fiber) = &self.fiber {
            fiber.borrow_mut().clear();
        }

        // Modules keep references to functions
        // functions keep references to modules.
        clear_maybe_module(self.core_module.take());
        clear_maybe_module(self.last_imported_module.take());
        for (_, module) in &self.modules {
            module.borrow_mut().clear();
        }

        // Classes hold onto Methods, which include Functions.
        clear_class(self.class_class.take());
        clear_class(self.fn_class.take());
        clear_class(self.fiber_class.take());

        if let Some(core) = self.core.take() {
            core.num.borrow_mut().clear();
            core.bool_class.borrow_mut().clear();
            core.null.borrow_mut().clear();
            core.string.borrow_mut().clear();
            core.range.borrow_mut().clear();
            core.list.borrow_mut().clear();
            core.map.borrow_mut().clear();
        }
    }
}

impl core::fmt::Debug for WrenVM {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WrenVM {{ ")?;
        if let Some(fiber) = &self.fiber {
            write!(f, "stack: {:?}, ", fiber.borrow())?;
        }
        write!(f, "methods: (len {}) ", self.methods.names.len())?;
        write!(f, "}}")
    }
}

pub(crate) fn wren_new_list(vm: &WrenVM, contents: Vec<Value>) -> Handle<ObjList> {
    new_handle(ObjList {
        class_obj: vm.core.as_ref().unwrap().list.clone(),
        elements: contents,
    })
}

pub(crate) fn wren_new_map(vm: &WrenVM) -> Handle<ObjMap> {
    new_handle(ObjMap {
        class_obj: vm.core.as_ref().unwrap().map.clone(),
        data: HashMap::new(),
    })
}

pub(crate) fn wren_new_range(
    vm: &WrenVM,
    from: f64,
    to: f64,
    is_inclusive: bool,
) -> Handle<ObjRange> {
    new_handle(ObjRange {
        class_obj: vm.core.as_ref().unwrap().range.clone(),
        from: from,
        to: to,
        is_inclusive: is_inclusive,
    })
}
pub(crate) fn wren_bind_superclass(subclass: &mut ObjClass, superclass: &Handle<ObjClass>) {
    subclass.superclass = Some(superclass.clone());
    // Setup fields
    // Inherit methods

    // Inherit methods from its superclass.
    // FIXME: Should this be in reverse order (to minimize # of resizes?)
    for (symbol, method) in superclass.borrow().methods.iter().enumerate() {
        subclass.set_method(symbol, method.clone());
    }
}

fn wren_new_single_class(source: ClassSource, name: String) -> Handle<ObjClass> {
    // the wren_c version does a lot more?  Unclear if this should.
    new_handle(ObjClass {
        name: name,
        methods: Vec::new(),
        class: None,
        superclass: None,
        source: source,
    })
}

// This was made in hopes of sharing code with base_class but it turns
// out this is an interpret time function (only creates classes)
// and does not do any of the declaration work base_class does.
// Keeping it for now in case it's useful later.
fn wren_new_class_with_class_class(
    superclass: &Handle<ObjClass>,
    source: ClassSource,
    name_string: String,
    class_class: &Handle<ObjClass>,
) -> Result<Handle<ObjClass>> {
    // Create the metaclass.

    let metaclass_name_string = format!("{} metaclass", name_string);
    // let metaclass_name = Value::from_string(metaclass_name_string);

    let metaclass = wren_new_single_class(ClassSource::Internal, metaclass_name_string);
    metaclass.borrow_mut().class = Some(class_class.clone());

    // Metaclasses always inherit Class and do not parallel the non-metaclass
    // hierarchy.
    wren_bind_superclass(&mut metaclass.borrow_mut(), class_class);

    let class = wren_new_single_class(source, name_string);
    class.borrow_mut().class = Some(metaclass);
    wren_bind_superclass(&mut class.borrow_mut(), superclass);

    Ok(class)
}

pub(crate) fn wren_new_class(
    vm: &WrenVM,
    superclass: &Handle<ObjClass>,
    source: ClassSource,
    name: String,
) -> Result<Handle<ObjClass>> {
    wren_new_class_with_class_class(superclass, source, name, &vm.class_class.as_ref().unwrap())
}

fn validate_superclass(
    name: &str,
    superclass_value: Value,
    source: &ClassSource,
) -> Result<Handle<ObjClass>> {
    // Make sure the superclass is a class.
    let superclass = superclass_value.try_into_class().ok_or_else(|| {
        VMError::from_string(format!(
            "Class '{}' cannot inherit from a non-class object.",
            name
        ))
    })?;

    // In wren_c, this is required since wren_c does blind-casts
    // of "this" in primitives.  wren_rust also does unwrap() and would
    // (safely) panic if "this" were a ObjInstance subclass.
    // FIXME: Merge with match below by checking ClassSource::Internal?
    match &superclass.borrow().name[..] {
        "Class" | "Fiber" | "Fn" | "List" | "Map" | "Range" | "String" | "Bool" | "Null"
        | "Num" => {
            return Err(VMError::from_string(format!(
                "Class '{}' cannot inherit from built-in class '{}'.",
                name,
                superclass.borrow().name
            )));
        }
        _ => {}
    }
    match (source, &superclass.borrow().source) {
        (_, ClassSource::Foreign) => {
            return Err(VMError::from_string(format!(
                "Class '{}' cannot inherit from foreign class '{}'.",
                name,
                superclass.borrow().name
            )));
        }
        (ClassSource::Foreign, ClassSource::Source(num_fields)) => {
            if *num_fields > 0 {
                return Err(VMError::from_string(format!(
                    "Foreign class '{}' may not inherit from a class with fields.",
                    name,
                )));
            }
        }
        (ClassSource::Source(fields), ClassSource::Source(super_fields)) => {
            if *fields + *super_fields > MAX_FIELDS {
                return Err(VMError::from_string(format!(
                    "Class '{}' may not have more than 255 fields, including inherited ones.",
                    name
                )));
            }
        }
        _ => {}
    }

    Ok(superclass)
}

fn bind_foreign_class(vm: &mut WrenVM, class: &mut ObjClass, module: &Module) {
    // Add the symbol even if there is no allocator so we can ensure that the
    // symbol itself is always in the symbol table.
    let allocate_symbol = vm.methods.ensure_symbol("<allocate>");
    // Add the symbol even if there is no finalizer so we can ensure that the
    // symbol itself is always in the symbol table.
    // let finalize_symbol = vm.methods.ensure_symbol("<finalize>");

    // Check the optional built-in module first so the host can override it.
    if let Some(bind_class) = vm.config.bind_foreign_class_fn {
        let methods = bind_class(vm, &module.name, &class.name);
        class.set_method(allocate_symbol, Method::ForeignFunction(methods.allocate));
        // if Some(finalize) = methods.finalize {
        //     class.set_method(finalize_symbol, Method::ForeignFunction(finalize));
        // }
    } else {
        // wren_c does not, but we could error here, since we know we have
        // no <allocate> for the class.
        // assert!("Allocator required for foreign classes");
    }

    // FIXME: Support opt_random.
}

fn create_class(
    vm: &mut WrenVM,
    frame: &mut CallFrame,
    source: ClassSource,
    module: &Module,
) -> Result<()> {
    // Pull the name and superclass off the stack.
    let superclass_value = frame.pop()?;
    let name_value = frame.pop()?;

    let name = name_value
        .try_into_string()
        .ok_or_else(|| VMError::from_str("Class name not string."))?;
    let superclass = validate_superclass(&name, superclass_value, &source)?;

    let class = wren_new_class(vm, &superclass, source.clone(), name)?;
    match source {
        ClassSource::Foreign => bind_foreign_class(vm, &mut class.borrow_mut(), module),
        _ => (),
    }
    // After bind_foreign_class to avoid a clone, should not make a differnce.
    frame.push(Value::Class(class));
    Ok(())
}

type Handle<T> = Rc<RefCell<T>>;

pub(crate) fn new_handle<T>(t: T) -> Handle<T> {
    Rc::new(RefCell::new(t))
}

#[derive(Debug)]
pub(crate) struct CoreClasses {
    pub(crate) num: Handle<ObjClass>,
    pub(crate) bool_class: Handle<ObjClass>,
    pub(crate) null: Handle<ObjClass>,
    pub(crate) string: Handle<ObjClass>,
    pub(crate) range: Handle<ObjClass>,
    pub(crate) list: Handle<ObjClass>,
    pub(crate) map: Handle<ObjClass>,
}

#[derive(Debug)]
pub enum DefinitionError {
    VariableAlreadyDefined,           // -1 in wren_c
    TooManyVariables,                 // -2 in wren_c
    LocalUsedBeforeDefinition(usize), // -3 in wren_c
}

impl From<ModuleLimitError> for DefinitionError {
    fn from(err: ModuleLimitError) -> DefinitionError {
        match err {
            ModuleLimitError::TooManyVariables => DefinitionError::TooManyVariables,
        }
    }
}

pub(crate) fn wren_define_variable(
    module: &mut Module,
    name: &str,
    value: Value,
) -> Result<u16, DefinitionError> {
    // See if the variable is already explicitly or implicitly declared.
    match module.lookup_symbol(name) {
        None => {
            // New variable!
            module.define_variable(name, value).map_err(From::from)
        }
        Some(symbol) => {
            let existing_value = &module.variables[symbol as usize];
            if let Some(line) = existing_value.try_into_num() {
                if wren_is_local_name(name) {
                    return Err(DefinitionError::LocalUsedBeforeDefinition(line as usize));
                }
                // An implicitly declared variable's value will always be a number.
                // Now we have a real definition.
                module.replace_implicit_definition(symbol, value);
                Ok(symbol)
            } else {
                Err(DefinitionError::VariableAlreadyDefined)
            }
        }
    }
}

// NOTE: This is only designed for Object and Class and does not fully
// wire up a class!
pub(crate) fn define_class(module: &mut Module, name: &str) -> Handle<ObjClass> {
    let class = new_handle(ObjClass {
        name: name.into(),
        methods: Vec::new(),
        class: None,
        superclass: None,
        source: ClassSource::Internal,
    });

    wren_define_variable(module, name, Value::Class(class.clone())).expect("defined");
    class
}

pub(crate) fn load_wren_core(vm: &mut WrenVM, module_name: &str) {
    // wren_core_source() is generated by build.rs from wren_core.wren
    let source = wren_core_source();
    let input = InputManager::from_str(source);
    let closure = compile_in_module(vm, module_name, input).expect("compile wren_core");
    // debug_bytecode(vm, &closure.borrow(), module);
    vm.run(closure).expect("run wren_core");
}

enum FunctionNext {
    Call(CallFrame),
    Return(Value),
    FiberAction(FiberAction),
}

// Looks up a foreign method in [moduleName] on [className] with [signature].
//
// This will try the host's foreign method binder first. If that fails, it
// falls back to handling the built-in modules.
fn find_foreign_method(
    vm: &WrenVM,
    module_name: &str,
    class_name: &str,
    is_static: bool,
    signature: &str,
) -> Option<WrenForeignMethodFn> {
    if let Some(bind_foreign_method) = vm.config.bind_foreign_method_fn {
        if let Some(method_fn) =
            bind_foreign_method(vm, module_name, class_name, is_static, signature)
        {
            return Some(method_fn);
        }
    }
    // FIXME: support opt_meta and opt_random modules.
    None
}

// Defines [methodValue] as a method on [classObj].
//
// Handles both foreign methods where [methodValue] is a string containing the
// method's signature and Wren methods where [methodValue] is a function.
//
// Aborts the current fiber if the method is a foreign method that could not be
// found.
fn bind_method(
    vm: &mut WrenVM,
    is_static: bool,
    symbol: usize,
    module: &Module,
    class: &mut ObjClass,
    method_value: Value,
) -> Result<()> {
    let method = match method_value {
        Value::String(signature) => {
            let module_name = &module.name;
            let class_name = &class.name;
            let foreign_fn =
                find_foreign_method(vm, module_name, class_name, is_static, &signature)
                    .ok_or_else(|| {
                        VMError::from_string(format!(
                            "Could not find foreign method '{}' for class {} in module '{}'.",
                            signature, class_name, module_name
                        ))
                    })?;
            Method::ForeignFunction(foreign_fn)
        }
        Value::Closure(closure) => Method::Block(closure),
        // Is this a compiler error?  Should this panic?
        _ => {
            return Err(VMError::from_str(
                "method value not a string (foreign) or closure (block",
            ))
        }
    };

    // FIXME: Need to patch the closure code!
    // Patch up the bytecode now that we know the superclass.
    // wrenBindMethodCode(classObj, method.as.closure->fn);

    if is_static {
        class.class_obj().borrow_mut().set_method(symbol, method);
    } else {
        class.set_method(symbol, method);
    };
    Ok(())
}

// Let the host resolve an imported module name if it wants to.
fn resolve_module(vm: &mut WrenVM, importer: &str, name: &str) -> String {
    if let Some(resolve_module) = vm.config.resolve_module_fn {
        resolve_module(vm, importer, name)
    } else {
        name.into()
    }
}

enum ImportResult {
    Existing(Handle<Module>),
    New(Handle<ObjClosure>),
}

fn import_module(
    vm: &mut WrenVM,
    importer_name: &str,
    unresolved_name: &str,
) -> Result<ImportResult> {
    let name = resolve_module(vm, importer_name, unresolved_name);

    // If the module is already loaded, we don't need to do anything.
    match vm.modules.get(&name) {
        Some(m) => return Ok(ImportResult::Existing(m.clone())),
        None => {}
    }

    // If we ever have other builtins, this doesn't need to be an error.
    let load_module = vm
        .config
        .load_module_fn
        .ok_or_else(|| VMError::from_string(format!("Could not load module '{}'.", name)))?;

    // Isn't there a rusty way to chain this with the above?
    let result = load_module(vm, &name)
        .ok_or_else(|| VMError::from_string(format!("Could not load module '{}'.", name)))?;

    let input = InputManager::from_string(result.source);
    let closure = compile_in_module(vm, &name, input)
        .map_err(|_| VMError::from_string(format!("Could not compile module '{}'.", name)))?;

    // Return the closure that executes the module.
    Ok(ImportResult::New(closure))
}

fn get_module_variable(_vm: &WrenVM, module: &Module, variable_name: &str) -> Result<Value> {
    module.variable_by_name(variable_name).ok_or_else(|| {
        VMError::from_string(format!(
            "Could not find a variable named '{}' in module '{}'.",
            variable_name, module.name
        ))
    })
}

impl WrenVM {
    pub fn new(config: WrenConfiguration) -> Self {
        let mut vm = Self {
            // Invalid import name, intentionally.
            methods: SymbolTable::default(),
            fiber: None,
            core_module: None,
            core: None,
            class_class: None,
            fn_class: None,
            fiber_class: None,
            modules: HashMap::new(),
            start_time: std::time::Instant::now(),
            last_imported_module: None,
            config: config,
            foreign_args: None,
        };

        // FIXME: This shouldn't be needed anymore.  We no longer
        // remove modules during parsing.
        // Modules are owned by the modules HashMap.
        // When compiled into, they are taken out of the HashMap.
        // Core is special.  We can't both compile core and have it
        // accessible for lookups at the same time, so we make this
        // stub, compile the real core, and then replace this.
        // FIXME: Does this mean that things compiled with the stub
        // will store their module-level variables in a different
        // module than the rest of core?
        let mut stub_core = Module {
            name: CORE_MODULE_NAME.into(),
            variables: Vec::new(),
            variable_names: Vec::new(),
        };
        init_base_classes(&mut vm, &mut stub_core);
        vm.class_class = Some(stub_core.expect_class("Class"));
        init_fn_and_fiber(&mut vm, &mut stub_core);
        vm.core_module = Some(new_handle(stub_core));
        let core_name = CORE_MODULE_NAME;
        load_wren_core(&mut vm, core_name);
        vm.core_module = Some(vm.modules.remove(core_name).unwrap());
        register_core_primitives(&mut vm);
        vm
    }

    pub(crate) fn lookup_or_register_empty_module(&mut self, name: &str) -> Handle<Module> {
        match self.modules.get(name) {
            Some(m) => return m.clone(),
            None => {}
        }
        let module = self.new_module_with_name(name);
        self.register_module(module.clone());
        module
    }

    pub(crate) fn new_module_with_name(&self, name: &str) -> Handle<Module> {
        // We automatically import Core into all modules.
        // Implicitly import the core module.
        let core = self.core_module.as_ref().unwrap().borrow();
        new_handle(Module {
            name: name.into(),
            variable_names: core.variable_names.clone(),
            variables: core.variables.clone(),
        })
    }

    pub(crate) fn register_module(&mut self, module: Handle<Module>) {
        let name = module.borrow().name.clone();
        self.modules.insert(name, module);
    }

    // called wrenGetClass in wren_c.
    pub(crate) fn class_for_value(&self, value: &Value) -> Handle<ObjClass> {
        let core = self.core.as_ref().unwrap();
        match value {
            Value::Null => core.null.clone(),
            Value::Num(_) => core.num.clone(),
            Value::Boolean(_) => core.bool_class.clone(),
            Value::String(_) => core.string.clone(),
            Value::Class(o) => o.borrow().class_obj(),
            Value::List(o) => o.borrow().class_obj(),
            Value::Map(o) => o.borrow().class_obj(),
            Value::Range(o) => o.borrow().class_obj(),
            Value::Fiber(o) => o.borrow().class_obj(),
            Value::Fn(o) => o.borrow().class_obj(),
            Value::Closure(o) => o.borrow().class_obj(),
            Value::Instance(o) => o.borrow().class_obj(),
        }
    }

    #[allow(dead_code)]
    fn print_methods(&self, class: &ObjClass) {
        println!("{:?} has:", class);
        for (symbol, method) in class.methods.iter().enumerate() {
            match method {
                Method::None => (),
                _ => {
                    println!(
                        "{} ({}) => {:?}",
                        self.methods.name_for_symbol(symbol),
                        symbol,
                        method
                    );
                }
            }
        }
    }

    fn method_not_found(&self, class: &ObjClass, symbol: usize) -> VMError {
        let name = self.methods.name_for_symbol(symbol);
        VMError::from_string(format!("{} does not implement '{}'.", class.name, name))
    }

    // Will this eventually have to walk across fibers?
    // Or can this just be a free function?
    fn stack_trace(&self, fiber: &ObjFiber) -> StackTrace {
        // Walk the fibers in reverse info.
        fn frame_info(frame: &CallFrame) -> FrameInfo {
            let closure = frame.closure.borrow();
            let fn_obj = closure.fn_obj.borrow();
            let module = fn_obj.module.borrow().name.clone();
            FrameInfo {
                module: module,
                // In an executing frame, the pc points to the *next*
                // instruction to execute, the error is from the
                // previous instruction.
                line: fn_obj.debug.line_for_pc(frame.pc - 1),
                fn_name: fn_obj.debug.name.clone(),
            }
        }

        StackTrace {
            frames: fiber
                .call_stack
                .borrow()
                .iter()
                .rev()
                .map(frame_info)
                .collect(),
        }
    }

    fn call_foreign(&mut self, foreign: &WrenForeignMethodFn, args: Vec<Value>) -> Value {
        assert!(
            self.foreign_args.is_none(),
            "Cannot already be in foreign call."
        );
        self.foreign_args = Some(args);
        foreign(self);
        // Take the value w/o copy
        let args = self.foreign_args.take();
        let value = args.unwrap().into_iter().nth(0).unwrap();
        value
    }

    fn cascade_error(&mut self, error: Value) -> Result<(), RuntimeError> {
        let stack_trace_fiber = self.fiber.clone().unwrap();
        loop {
            let callee = self.fiber.clone().unwrap();
            // Set Fiber.error on the current fiber. Can't do this
            // deeper in the stack because can't borrow_mut there.
            callee.borrow_mut().error = error.clone();
            // If we have a caller, it's now the new fiber.
            let caller = match &self.fiber {
                Some(fiber) => fiber.borrow_mut().return_from_fiber_take_caller(),
                _ => None,
            };
            self.fiber = caller;

            match &self.fiber {
                // If the previously called fiber was a try, return
                // control and the error value.
                Some(fiber) => {
                    if callee.borrow().is_try() {
                        fiber.borrow_mut().push_value_to_top_of_stack(error);
                        break;
                    }
                    // If this wasn't a try, continue walking up
                    // the fiber stack.
                }
                // If we've reached the root fiber, return.
                None => {
                    return Err(RuntimeError::from_error_value(
                        error,
                        self.stack_trace(&stack_trace_fiber.borrow()),
                    ))
                }
            }
        }
        Ok(())
    }

    pub(crate) fn run(&mut self, closure: Handle<ObjClosure>) -> Result<Value, RuntimeError> {
        self.fiber = Some(new_handle(ObjFiber::new(
            self,
            closure,
            FiberRunSource::Root,
        )));
        loop {
            let fiber = self.fiber.clone().unwrap();
            let result = self.run_in_fiber(&fiber.borrow());
            match result {
                Ok(FiberAction::Call(fiber, arg)) => {
                    fiber.borrow_mut().caller = self.fiber.take();
                    fiber.borrow_mut().push_value_to_top_of_stack(arg);
                    self.fiber = Some(fiber.clone());
                }
                Ok(FiberAction::Try(fiber, arg)) => {
                    fiber.borrow_mut().caller = self.fiber.take();
                    fiber.borrow_mut().push_value_to_top_of_stack(arg);
                    fiber.borrow_mut().run_source = FiberRunSource::Try;
                    self.fiber = Some(fiber.clone());
                }
                Ok(FiberAction::Suspend) => {
                    self.fiber = None;
                    // FIXME: This return value is wrong.
                    // The api should not return a value for Fiber.suspend.
                    return Ok(Value::Null);
                }
                Ok(FiberAction::Return(value)) => {
                    let caller = if let Some(fiber) = &self.fiber {
                        fiber.borrow_mut().return_from_fiber_take_caller()
                    } else {
                        // This should never be reached?
                        None
                    };
                    self.fiber = caller;
                    match &self.fiber {
                        Some(fiber) => fiber.borrow_mut().push_value_to_top_of_stack(value),
                        None => return Ok(value),
                    }
                }
                Ok(FiberAction::Transfer(fiber, arg)) => {
                    fiber.borrow_mut().push_value_to_top_of_stack(arg);
                    self.fiber = Some(fiber.clone());
                }
                Ok(FiberAction::TransferError(fiber, error)) => {
                    self.fiber = Some(fiber);
                    self.cascade_error(error)?;
                }
                Err(error) => {
                    self.cascade_error(error.as_try_return_value())?;
                }
            }
        }
    }

    fn run_in_fiber(&mut self, fiber: &ObjFiber) -> Result<FiberAction, VMError> {
        loop {
            // FIXME: I suspect we no longer need to pop the active frame since
            // the call_stack is held in a RefCell and can be passed with
            // separate mutability from ObjFiber?
            let mut frame = fiber.call_stack.borrow_mut().pop().unwrap();
            // This is all to avoid run_fiber needing to call itself
            // recursively, or the run_fiber main loop needing to pull
            // the frame on every instruction.  Maybe not worth it?
            let result = self.run_frame_in_fiber(&mut frame, fiber);
            match result {
                Ok(FunctionNext::Call(new_frame)) => {
                    let mut call_stack = fiber.call_stack.borrow_mut();
                    // call_stack does not contain "frame", restore it.
                    call_stack.push(frame);
                    // Now push our new frame!
                    call_stack.push(new_frame);
                }
                Ok(FunctionNext::Return(value)) => {
                    if fiber.call_stack.borrow().is_empty() {
                        return Ok(FiberAction::Return(value));
                    } else {
                        // Take the return value and push it onto the calling stack.
                        fiber
                            .call_stack
                            .borrow_mut()
                            .last_mut()
                            .unwrap()
                            .push(value);
                    }
                }
                Ok(FunctionNext::FiberAction(action)) => {
                    fiber.call_stack.borrow_mut().push(frame); // FIXME?
                    return Ok(action);
                }
                Err(vm_error) => {
                    // Push the current frame back onto the fiber so we can
                    // see it in error reporting.
                    fiber.call_stack.borrow_mut().push(frame);
                    return Err(vm_error);
                }
            }
        }
    }

    fn should_dump_instructions(&self, fn_debug: &FnDebug) -> bool {
        match &self.config.debug_level {
            None => false,
            Some(level) => match level {
                DebugLevel::All => true,
                DebugLevel::NonCore => !fn_debug.from_core_module,
            },
        }
    }

    fn create_foreign(&mut self, class_handle: &Handle<ObjClass>) -> Result<Value> {
        // wren_c makes these all asserts, but I'm not sure why?
        fn vm_assert(condition: bool, msg: &str) -> Result<()> {
            if condition {
                Err(VMError::from_str(msg))
            } else {
                Ok(())
            }
        }

        let class = class_handle.borrow();
        vm_assert(class.is_foreign(), "Class must be a foreign class.")?;

        // wren_c TODO: Don't look up every time.
        let symbol = self
            .methods
            .symbol_for_name("<allocate>")
            .ok_or_else(|| VMError::from_str("Should have defined <allocate> symbol."))?;

        vm_assert(class.methods.len() > symbol, "Class should have allocator.")?;
        let method = &class.methods[symbol];

        // Pass the constructor arguments to the allocator as well.
        match method {
            Method::ForeignFunction(foreign) => {
                let args = vec![Value::Class(class_handle.clone())];
                Ok(self.call_foreign(foreign, args))
            }
            _ => Err(VMError::from_str("Allocator should be foreign.")),
        }
    }

    fn run_frame_in_fiber(
        &mut self,
        frame: &mut CallFrame,
        fiber: &ObjFiber,
    ) -> Result<FunctionNext> {
        // Copy out a ref, so we can later mut borrow the frame.
        let closure_rc = frame.closure.clone();
        let closure = closure_rc.borrow();
        let fn_obj = closure.fn_obj.borrow();
        frame.pc;
        let dump_instructions = self.should_dump_instructions(&fn_obj.debug);
        loop {
            let op = &fn_obj.code[frame.pc];
            if dump_instructions {
                // Grab the "dynamic" scope (top module) since that
                // makes the most sense as the "owner" of the stack.
                let top_module_name = match &fiber.call_stack.borrow().first() {
                    None => module_name_and_line(&fn_obj, frame.pc),
                    Some(top_frame) => {
                        module_name_and_line(&top_frame.closure.borrow().fn_obj.borrow(), frame.pc)
                    }
                };
                let frame_depth = fiber.call_stack.borrow().len(); // Does not include current frame.
                frame.dump_stack(&top_module_name, frame_depth);
                dump_instruction(
                    frame.pc,
                    op,
                    &self.methods,
                    &frame.closure.borrow(),
                    Some(frame),
                    DumpMode::ShowSymbols,
                );
            }
            frame.pc += 1;
            match op {
                Ops::Constant(index) => {
                    frame.push(fn_obj.constants[*index].clone());
                }
                Ops::Boolean(value) => {
                    frame.push(Value::Boolean(*value));
                }
                Ops::Null => {
                    frame.push(Value::Null);
                }
                Ops::Call(signature, symbol) => {
                    // Implicit arg for this.
                    let num_args = signature.arity as usize + 1;
                    let this_offset = frame.stack.len() - num_args;
                    // Unlike wren_c, we remove the args from the stack
                    // and pass them to the function.
                    let args = frame.stack.split_off(this_offset);

                    let this_class = self.class_for_value(&args[0]);
                    let class_obj = this_class.borrow();
                    // Get the Method record for this class for this symbol.
                    // This could return None instead of MethodNone?
                    let method = class_obj
                        .methods
                        .get(*symbol)
                        .ok_or_else(|| self.method_not_found(&class_obj, *symbol))?;

                    // Even if we got a Method doesn't mean *this* class
                    // implements it.
                    match method {
                        Method::ValuePrimitive(f) => {
                            let result = f(self, args)?;
                            frame.push(result);
                        }
                        Method::FiberActionPrimitive(f) => {
                            let action = f(self, args)?;
                            return Ok(FunctionNext::FiberAction(action));
                        }
                        Method::FunctionCall => {
                            // Pushes a new stack frame.
                            // wren_rust (currently) keeps a separate stack
                            // per CallFrame, so underflows are caught on a
                            // per-function call basis.
                            return Ok(FunctionNext::Call(CallFrame {
                                pc: 0,
                                closure: check_arity(&args[0], num_args)?,
                                stack: args,
                            }));
                        }
                        Method::ForeignFunction(foreign) => {
                            self.call_foreign(foreign, args);
                        }
                        Method::Block(closure) => {
                            // Pushes a new stack frame.
                            // wren_rust (currently) keeps a separate stack
                            // per CallFrame, so underflows are caught on a
                            // per-function call basis.
                            // println!("About to call:");
                            // debug_bytecode(self, &closure.borrow(), module);
                            return Ok(FunctionNext::Call(CallFrame {
                                pc: 0,
                                closure: closure.clone(),
                                stack: args,
                            }));
                        }
                        // FIXME: This should use Option<Method> instead.
                        Method::None => {
                            // This is the case where the methods vector was
                            // large enough to have the symbol (because the
                            // class supports a larger symbol), but this symbol
                            // is Method::None (not implemented).
                            return Err(self.method_not_found(&class_obj, *symbol));
                        }
                    }
                }
                Ops::Construct => {
                    let this = frame.stack[0].clone();
                    let class = this.try_into_class().expect("'this' should be a class.");
                    let instance = Value::Instance(new_handle(ObjInstance::new(class)));
                    frame.stack[0] = instance;
                }
                Ops::ForeignConstruct => {
                    let this = frame.stack[0].clone();
                    let class = this.try_into_class().expect("'this' should be a class.");
                    frame.stack[0] = self.create_foreign(&class)?;
                }
                Ops::Closure(constant_index, _upvalues) => {
                    let fn_value = fn_obj.constants[*constant_index].clone();
                    let fn_obj = fn_value
                        .try_into_fn()
                        .ok_or_else(|| VMError::from_str("constant was not closure"))?;
                    let closure = new_handle(ObjClosure::new(self, fn_obj));
                    frame.push(Value::Closure(closure));
                    // FIXME: Handle upvalues.
                }
                Ops::EndModule => {
                    self.last_imported_module = Some(fn_obj.module.clone());
                    frame.push(Value::Null); // Is this the return value?
                }
                Ops::Return => {
                    // The top of our stack is returned and then the caller of
                    // this rust function will push the return onto the stack of
                    // the calling wren function CallFrame.
                    return Ok(FunctionNext::Return(frame.pop()?));
                }
                Ops::Class(num_fields) => {
                    create_class(
                        self,
                        frame,
                        ClassSource::Source(*num_fields),
                        &fn_obj.module.borrow(),
                    )?;
                }
                Ops::ForeignClass => {
                    create_class(self, frame, ClassSource::Foreign, &fn_obj.module.borrow())?;
                }
                Ops::Load(variable) => {
                    let value = match variable.scope {
                        Scope::Module => fn_obj.module.borrow().variables[variable.index].clone(),
                        Scope::Upvalue => unimplemented!("load upvalue"),
                        Scope::Local => frame.stack[variable.index].clone(),
                    };
                    frame.push(value);
                }
                Ops::Store(variable) => {
                    let value = frame.peek()?;
                    match variable.scope {
                        Scope::Module => {
                            fn_obj.module.borrow_mut().variables[variable.index] = value.clone()
                        }
                        Scope::Upvalue => unimplemented!("store upvalue"),
                        Scope::Local => frame.stack[variable.index] = value.clone(),
                    };
                }
                Ops::LoadField(symbol) => {
                    let receiver = frame.pop()?;
                    let instance = receiver
                        .try_into_instance()
                        .ok_or_else(|| VMError::from_str("Receiver should be instance."))?;
                    if *symbol >= instance.borrow().fields.len() {
                        return Err(VMError::from_str("Out of bounds field."));
                    }
                    frame.push(instance.borrow().fields[*symbol].clone());
                }
                Ops::StoreField(symbol) => {
                    let receiver = frame.pop()?;
                    let instance = receiver
                        .try_into_instance()
                        .ok_or_else(|| VMError::from_str("Receiver should be instance."))?;
                    if *symbol >= instance.borrow().fields.len() {
                        return Err(VMError::from_str("Out of bounds field."));
                    }
                    instance.borrow_mut().fields[*symbol] = frame.peek()?.clone();
                }
                Ops::Pop => {
                    frame.pop()?;
                }
                Ops::Method(is_static, symbol) => {
                    // wren_c peeks first then drops after bind, unclear why
                    let class = frame.pop()?.try_into_class().unwrap();
                    let method = frame.pop()?;
                    bind_method(
                        self,
                        *is_static,
                        *symbol,
                        &fn_obj.module.borrow(),
                        &mut class.borrow_mut(),
                        method,
                    )?;
                }
                Ops::End => {
                    panic!("Compile error: END instruction should never be reached.");
                }
                Ops::ImportModule(module_name) => {
                    // Make a slot on the stack for the module's fiber to place the return
                    // value. It will be popped after this fiber is resumed. Store the
                    // imported module's closure in the slot in case a GC happens when
                    // invoking the closure.
                    let result = import_module(self, &fn_obj.module.borrow().name, module_name)?;
                    // frame.push(value.clone());
                    // Check for fiber error?
                    // if wrenHasError(fiber) RUNTIME_ERROR();

                    // If we get a closure, call it to execute the module body.
                    match result {
                        ImportResult::New(closure) => {
                            return Ok(FunctionNext::Call(CallFrame::new_root(closure)));
                        }
                        ImportResult::Existing(module) => {
                            // The module has already been loaded. Remember it so we can import
                            // variables from it if needed.
                            frame.push(Value::from_str("dummy for module"));
                            self.last_imported_module = Some(module);
                        }
                    }
                }
                Ops::ImportVariable(variable_name) => {
                    let module = self
                        .last_imported_module
                        .as_ref()
                        .expect("Should have already imported module.");
                    let value = get_module_variable(self, &module.borrow(), variable_name)?;
                    frame.push(value);
                }
                Ops::Loop(offset_backwards) => {
                    frame.pc -= *offset_backwards as usize;
                }
                Ops::Jump(offset_forward) => {
                    frame.pc += *offset_forward as usize;
                }
                Ops::JumpIfFalse(offset_forward) => {
                    let value = frame.pop()?;
                    if value.is_falsey() {
                        frame.pc += *offset_forward as usize;
                    }
                }
                Ops::And(offset_forward) => {
                    // This differs from JumpIfFalse in whether it pops
                    let value = frame.peek()?;
                    if value.is_falsey() {
                        frame.pc += *offset_forward as usize;
                    } else {
                        frame.pop()?;
                    }
                }
                Ops::Or(offset_forward) => {
                    let value = frame.peek()?;
                    if !value.is_falsey() {
                        frame.pc += *offset_forward as usize;
                    } else {
                        frame.pop()?;
                    }
                }
                Ops::ClassPlaceholder => unimplemented!(),
                Ops::JumpIfFalsePlaceholder => unimplemented!(),
                Ops::JumpPlaceholder => unimplemented!(),
                Ops::OrPlaceholder => unimplemented!(),
                Ops::AndPlaceholder => unimplemented!(),
            }
        }
    }
}

fn check_arity(value: &Value, num_args: usize) -> Result<Handle<ObjClosure>> {
    let closure = value
        .try_into_closure()
        .expect("Receiver must be a closure.");

    // num_args includes implicit this, not counted in arity.
    let arity = closure.borrow().fn_obj.borrow().arity as usize;
    if num_args >= arity + 1 {
        Ok(closure)
    } else {
        Err(VMError::from_str("Function expects more arguments."))
        // I guess too many arguments is OK?
    }
}

impl CallFrame {
    // FIXME: push should be private.
    pub(crate) fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    // FIXME: No longer needs to be Result, now that Stack Underflow panics
    fn pop(&mut self) -> Result<Value> {
        Ok(self.stack.pop().expect("Stack underflow"))
    }

    // FIXME: No longer needs to be Result, now that Stack Underflow panics
    fn peek(&mut self) -> Result<&Value> {
        Ok(self.stack.last().expect("Stack underflow"))
    }

    fn dump_stack(&self, active_module: &str, frame_depth: usize) {
        // Print the stack left (top) to right (bottom)
        let mut as_string = Vec::new();
        for (index, value) in self.stack.iter().enumerate().rev() {
            let debug = format!("{:?}", value);
            as_string.push(format!("{}: {:10}", index, debug));
        }
        as_string.reverse();
        println!(
            "{:10}  Stack({:2}): {}",
            active_module,
            frame_depth,
            as_string.join(" ")
        );
    }
}

enum DumpMode {
    ShowSymbols,
    HideSymbols,
}

fn module_name_and_line(fn_obj: &ObjFn, pc: usize) -> String {
    format!(
        "{}:{}",
        std::path::Path::new(&fn_obj.module.borrow().name)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        fn_obj.debug.line_for_pc(pc)
    )
}

fn dump_instruction(
    pc: usize,
    op: &Ops,
    methods: &SymbolTable,
    closure: &ObjClosure,
    frame: Option<&CallFrame>,
    mode: DumpMode,
) {
    let fn_obj = &closure.fn_obj.borrow();
    println!(
        "{}:{} {:02}: {}",
        module_name_and_line(fn_obj, pc),
        fn_obj.debug.name,
        pc,
        op_debug_string(op, methods, closure, frame, mode)
    );
}

fn op_debug_string(
    op: &Ops,
    methods: &SymbolTable,
    closure: &ObjClosure,
    frame: Option<&CallFrame>,
    mode: DumpMode,
) -> String {
    // If stable_output do not print the symbol, otherwise every time we
    // change wren_core.wren all compile.txt files change.
    let unstable_num_arrow = |symbol: usize| match mode {
        DumpMode::HideSymbols => format!(""),
        DumpMode::ShowSymbols => format!("{} -> ", symbol),
    };
    let comma_unstable_num = |symbol: usize| match mode {
        DumpMode::HideSymbols => format!(""),
        DumpMode::ShowSymbols => format!(" {}", symbol),
    };

    let fn_obj = closure.fn_obj.borrow();

    match op {
        Ops::Constant(i) => format!("Constant({}: {:?})", i, fn_obj.constants[*i]),
        Ops::Boolean(b) => format!("Boolean {}", b),
        Ops::Null => format!("{:?}", op),
        Ops::Call(sig, symbol) => {
            format!(
                "Call({:?}, {}{})",
                sig.call_type,
                unstable_num_arrow(*symbol),
                methods.name_for_symbol(*symbol)
            )
        }
        Ops::Load(v) => match v.scope {
            Scope::Local => match frame {
                // Do not hide symbols for locals, they're stable-enough.
                Some(f) => format!("Load(Local, {} -> {:?})", v.index, f.stack[v.index]),
                None => format!("Load(Local, {})", v.index),
            },
            Scope::Upvalue => unimplemented!("dump load upvalue"),
            Scope::Module => {
                format!(
                    "Load(Module, {}{:?})",
                    unstable_num_arrow(v.index),
                    fn_obj.module.borrow().variables[v.index]
                )
            }
        },
        Ops::Store(v) => match v.scope {
            Scope::Local => format!("Store(Local, {})", v.index),
            Scope::Upvalue => unimplemented!("dump store upvalue"),
            Scope::Module => format!("Store(Module{})", comma_unstable_num(v.index)),
        },
        Ops::LoadField(field) => format!("LoadField({})", field),
        Ops::StoreField(field) => format!("StoreField({})", field),
        Ops::JumpIfFalsePlaceholder => format!("{:?}", op),
        Ops::JumpIfFalse(_) => format!("{:?}", op),
        Ops::JumpPlaceholder => format!("{:?}", op),
        Ops::And(_) => format!("{:?}", op),
        Ops::AndPlaceholder => format!("{:?}", op),
        Ops::Or(_) => format!("{:?}", op),
        Ops::OrPlaceholder => format!("{:?}", op),
        Ops::ClassPlaceholder => format!("{:?}", op),
        Ops::Construct => format!("{:?}", op),
        Ops::ForeignConstruct => format!("{:?}", op),
        Ops::Method(is_static, symbol) => {
            let dispatch = if *is_static { "static" } else { "instance" };
            format!(
                "Method({}, {}{})",
                dispatch,
                unstable_num_arrow(*symbol),
                methods.name_for_symbol(*symbol)
            )
        }
        Ops::Closure(_, _) => format!("{:?}", op),
        Ops::Class(num_fields) => format!("Class({} fields)", num_fields),
        Ops::ForeignClass => format!("{:?}", op),
        Ops::Jump(_) => format!("{:?}", op),
        Ops::Loop(_) => format!("{:?}", op),
        Ops::Pop => format!("{:?}", op),
        Ops::Return => format!("{:?}", op),
        Ops::EndModule => format!("{:?}", op),
        Ops::End => format!("{:?}", op),
        Ops::ImportModule(_) => format!("{:?}", op),
        Ops::ImportVariable(_) => format!("{:?}", op),
    }
}

pub(crate) fn wren_debug_bytecode(vm: &WrenVM, closure: &ObjClosure) {
    debug_bytecode(vm, closure);
}

fn debug_bytecode(vm: &WrenVM, closure: &ObjClosure) {
    let func = &closure.fn_obj.borrow();
    println!("Constants:");
    for (id, value) in func.constants.iter().enumerate() {
        println!("{:02}: {:?}", id, value);
    }
    println!("Code:");
    for (pc, op) in func.code.iter().enumerate() {
        // Don't share with dump_instruction for now.
        println!(
            "{:02} (ln {}): {}",
            pc,
            func.debug.line_for_pc(pc),
            op_debug_string(op, &vm.methods, closure, None, DumpMode::HideSymbols)
        );
    }
    // Walk module-level constants looking for code?
    // Walk functions/closures looking for code?
}

pub trait Obj: Clear {
    // The object's class.
    fn class_obj(&self) -> Handle<ObjClass>;
}

pub(crate) struct ObjRange {
    class_obj: Handle<ObjClass>,
    // The beginning of the range.
    pub(crate) from: f64,
    // The end of the range. May be greater or less than [from].
    pub(crate) to: f64,
    // True if [to] is included in the range.
    pub(crate) is_inclusive: bool,
}

impl ObjRange {
    pub(crate) fn min(&self) -> f64 {
        self.from.min(self.to)
    }

    pub(crate) fn max(&self) -> f64 {
        self.from.max(self.to)
    }
}

pub(crate) struct ObjMap {
    class_obj: Handle<ObjClass>,
    pub(crate) data: HashMap<Value, Value>,
}

impl Obj for ObjMap {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

pub(crate) struct ObjList {
    class_obj: Handle<ObjClass>,
    pub(crate) elements: Vec<Value>,
}

impl Obj for ObjList {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

pub(crate) struct ObjClosure {
    class_obj: Handle<ObjClass>,
    pub(crate) fn_obj: Handle<ObjFn>,
}

impl ObjClosure {
    pub(crate) fn new(vm: &WrenVM, fn_obj: Handle<ObjFn>) -> ObjClosure {
        // FIXME: Is this really supposed to also be class = fn?
        ObjClosure {
            class_obj: vm.fn_class.as_ref().unwrap().clone(),
            fn_obj: fn_obj,
        }
    }
}

impl Obj for ObjClosure {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

pub(crate) struct ObjFn {
    class_obj: Handle<ObjClass>,
    pub(crate) arity: u8,
    pub(crate) constants: Vec<Value>,
    pub(crate) code: Vec<Ops>,
    pub(crate) debug: FnDebug,
    // This is needed so that we can find the Module through which to
    // do module-level loads/stores when executing.  This is *definitely*
    // circular reference however.  Maybe there is another way?
    pub(crate) module: Handle<Module>,
}

impl ObjFn {
    pub(crate) fn new(
        vm: &WrenVM,
        module: Handle<Module>,
        compiler: Box<crate::compiler::Compiler>,
        arity: u8,
    ) -> ObjFn {
        ObjFn {
            class_obj: vm.fn_class.as_ref().unwrap().clone(),
            constants: compiler.constants.list,
            code: compiler.code,
            arity: arity,
            debug: compiler.fn_debug,
            module: module,
        }
    }
}

impl Obj for ObjFn {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

pub(crate) struct ObjInstance {
    class_obj: Handle<ObjClass>,
    fields: Vec<Value>,
}

impl ObjInstance {
    pub(crate) fn new(class: Handle<ObjClass>) -> ObjInstance {
        let num_fields = class
            .borrow()
            .num_fields()
            .expect("Compiler emitted Construct for non-source class.");
        let fields = vec![Value::Null; num_fields];
        ObjInstance {
            class_obj: class,
            fields: fields,
        }
    }
}

impl Obj for ObjInstance {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

impl core::fmt::Debug for ObjRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let op_string = if self.is_inclusive { ".." } else { "..." };
        write!(f, "{}{}{}", self.from, op_string, self.to)
    }
}

impl Obj for ObjRange {
    fn class_obj(&self) -> Handle<ObjClass> {
        self.class_obj.clone()
    }
}

// impl core::fmt::Debug for dyn Obj {
//     fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
//         write!(f, "Object{:?}", self.obj_type())
//     }
// }

pub(crate) enum FiberAction {
    Call(Handle<ObjFiber>, Value),
    Transfer(Handle<ObjFiber>, Value),
    TransferError(Handle<ObjFiber>, Value),
    // AFAICT, Return and Yield are the same.
    Return(Value),
    Try(Handle<ObjFiber>, Value),
    Suspend,
}

// Unclear if this should take Vec or a slice?
type ValuePrimitive = fn(vm: &WrenVM, args: Vec<Value>) -> Result<Value>;
type FiberActionPrimitive = fn(vm: &WrenVM, args: Vec<Value>) -> Result<FiberAction>;

#[derive(Clone)]
pub(crate) enum Method {
    // A primitive method implemented in C in the VM. Unlike foreign methods,
    // this can directly manipulate the fiber's stack.
    ValuePrimitive(ValuePrimitive),
    FiberActionPrimitive(FiberActionPrimitive),

    // A primitive that handles .call on Fn.
    FunctionCall,

    // A externally-defined method.
    ForeignFunction(WrenForeignMethodFn),

    // A normal user-defined method.
    Block(Handle<ObjClosure>),

    // No method for the given symbol.
    None,
}

impl core::fmt::Debug for Method {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Method::ValuePrimitive(_) => write!(f, "ValuePrimitive"),
            Method::FiberActionPrimitive(_) => write!(f, "FiberActionPrimitive"),
            Method::FunctionCall => write!(f, "FunctionCall"),
            Method::Block(closure) => {
                write!(f, "Block {}", closure.borrow().fn_obj.borrow().debug.name)
            }
            Method::ForeignFunction(_) => write!(f, "ForeignFunction"),
            Method::None => write!(f, "None"),
        }
    }
}
#[derive(Clone)]
pub(crate) enum ClassSource {
    Internal,      // vm defined, no fields.
    Source(usize), // source defined, fields allowed
    Foreign,       // api defined, fields opaque to vm.
}

// Implement Default to allow take() to work during clear().
impl Default for ClassSource {
    fn default() -> Self {
        ClassSource::Internal
    }
}

// Derive from Default to allow take() to work during clear().
#[derive(Default)]
pub struct ObjClass {
    // Class is option Option due to Object and Class and Object metaclass
    // initialization starting with class = None and then filling in.
    // The Option is also used during clear() teardown during WrenVM::drop.
    // We could use a dummy class to start intead and make this non-optional.
    pub(crate) class: Option<Handle<ObjClass>>,

    // Class is the only class w/o a superclass, all others this is Some(class).
    pub(crate) superclass: Option<Handle<ObjClass>>,

    // What created this class.  Including the number of fields it has if it
    // was created from source, rather than internal or foreign.
    source: ClassSource,

    // The table of methods that are defined in or inherited by this class.
    // Methods are called by symbol, and the symbol directly maps to an index in
    // this table. This makes method calls fast at the expense of empty cells in
    // the list for methods the class doesn't support.
    //
    // You can think of it as a hash table that never has collisions but has a
    // really low load factor. Since methods are pretty small (just a type and a
    // pointer), this should be a worthwhile trade-off.
    methods: Vec<Method>,

    // The name of the class.
    pub(crate) name: String, // Should be Rc<ObjString>?

                             // The ClassAttribute for the class, if any
                             //   Value attributes;
}

impl ObjClass {
    fn num_fields(&self) -> Option<usize> {
        match self.source {
            ClassSource::Source(num_fields) => Some(num_fields),
            _ => None, // Not a valid question to ask.
        }
    }

    fn is_foreign(&self) -> bool {
        match self.source {
            ClassSource::Foreign => true,
            _ => false,
        }
    }
}

// FIXME: This is a hack?  for object_is
// impl PartialEq for ObjClass {
//     fn eq(&self, other: &ObjClass) -> bool {
//         self.name.eq(&other.name)
//     }
// }

impl core::fmt::Debug for ObjClass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ObjClass {{ {} ", self.name)?;
        match &self.superclass {
            None => write!(f, "super: None ")?,
            // FIXME: This is another instance where having an iterator
            // over the class hierarchy could be useful to print
            // Bool->Object, etc.
            Some(rc) => write!(f, "super: {:?} ", rc.borrow().name)?,
        }
        match &self.class {
            None => write!(f, "meta: None ")?,
            Some(rc) => write!(f, "meta: {:?} ", rc.borrow().name)?,
        }
        write!(f, "}}")
    }
}

impl ObjClass {
    // wren_c calls this wrenBindMethod
    pub(crate) fn set_method(&mut self, symbol: usize, method: Method) {
        if symbol >= self.methods.len() {
            self.methods.resize(symbol + 1, Method::None);
        }
        self.methods[symbol] = method;
    }

    // pub(crate) fn lookup_method(&self, symbol: usize) -> Option<&Method> {
    //     self.methods.get(symbol)
    // }
}

impl Obj for ObjClass {
    fn class_obj(&self) -> Handle<ObjClass> {
        // FIXME: Fix to be non-Option and remove unwrap.
        self.class.as_ref().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn value_num_hash_unique() {
        fn hash_as_value(num: f64) -> u64 {
            use std::hash::{Hash, Hasher};
            let value = super::Value::Num(num);
            let mut s = std::collections::hash_map::DefaultHasher::new();
            value.hash(&mut s);
            s.finish()
        }
        assert_ne!(hash_as_value(1.0), hash_as_value(2.0));
        assert_eq!(hash_as_value(1.0), hash_as_value(1.0));
        assert_eq!(hash_as_value(3.0), hash_as_value(1.0 + 2.0));
        // Floats equality is hard.  We don't try to fix that:
        assert_ne!(hash_as_value(0.3), hash_as_value(0.1 + 0.2));
    }
}
