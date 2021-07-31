use wren_rust::wren::{WrenForeignMethodFn, WrenVM};

fn runtime_error(vm: &mut WrenVM) {
    vm.ensure_slots(1);
    vm.set_slot_string(0, "Error!");
    vm.abort_fiber(0);
}

pub fn error_bind_method(signature: &str) -> Option<WrenForeignMethodFn> {
    if signature == "static Error.runtimeError" {
        Some(runtime_error)
    } else {
        None
    }
}