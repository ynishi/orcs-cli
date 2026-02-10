pub mod layer_dep;
pub mod no_panic;
pub mod no_unwrap;

pub use layer_dep::check as check_layer_dep;
pub use no_panic::NoPanicInLib;
pub use no_unwrap::NoUnwrap;

/// 全ルール一覧: (name, description)
pub const ALL_RULES: &[(&str, &str)] = &[
    (
        layer_dep::NAME,
        "Cargo.toml layer dependency direction check",
    ),
    (
        no_unwrap::NAME,
        ".unwrap() forbidden everywhere, .expect() forbidden in prod (allowed in tests)",
    ),
    (
        no_panic::NAME,
        "panic!/todo!/unimplemented! forbidden in library code",
    ),
];
