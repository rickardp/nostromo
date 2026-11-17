extern "C" { fn native_foo(x: i32) -> i32; }
pub fn foo(x: i32) -> i32 { unsafe { native_foo(x) } }
