use pm::Hello;
#[derive(Hello, serde::Serialize)]
struct Thing { v: u32 }
fn main() {
    if std::env::args().any(|a| a == "panic") { panic!("deliberate"); }
    let t = Thing { v: gen::GENERATED + app::leaf() };
    println!("hello={} json={} foo={} re={} gen_dir_ok={} runtime_data={:?} header_ok={}",
        Thing::hello(), serde_json::to_string(&t).unwrap(), consumer::call(5),
        "123".chars().all(|c| c.is_ascii_digit()), std::path::Path::new(gen::GEN_DIR).exists(),
        gen::runtime_data().ok().map(|s| s.trim().to_string()), consumer::header_exists());
}
