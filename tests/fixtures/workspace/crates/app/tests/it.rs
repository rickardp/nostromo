#[test] fn native_works() { assert_eq!(consumer::call(5), 16); }
#[test] fn generated() { assert_eq!(gen::GENERATED, 42); }
