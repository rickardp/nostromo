use proc_macro::TokenStream;

#[proc_macro_derive(Hello)]
pub fn hello(input: TokenStream) -> TokenStream {
    let name = input.into_iter().skip_while(|t| t.to_string() != "struct").nth(1).unwrap().to_string();
    format!("impl {name} {{ pub fn hello() -> &'static str {{ stringify!({name}) }} }}").parse().unwrap()
}
