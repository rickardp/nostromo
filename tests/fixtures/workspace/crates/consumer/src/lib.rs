pub const NATIVE_INCLUDE: &str = env!("NATIVE_INCLUDE");
pub fn header_exists() -> bool { std::path::Path::new(NATIVE_INCLUDE).join("foo.h").exists() }
pub fn call(x: i32) -> i32 { native::foo(x) }
