include!(concat!(env!("OUT_DIR"), "/gen.rs"));
pub const GEN_DIR: &str = env!("GEN_DIR");
pub fn runtime_data() -> std::io::Result<String> { std::fs::read_to_string(format!("{}/data.txt", GEN_DIR)) }
