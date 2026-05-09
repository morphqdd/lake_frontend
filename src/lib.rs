#![deny(
    clippy::unwrap_used,
    clippy::get_unwrap,
    clippy::panicking_unwrap,
    clippy::unwrap_in_result,
    clippy::unnecessary_literal_unwrap,
    clippy::unnecessary_unwrap,
    clippy::or_then_unwrap
)]

pub mod api;
pub mod error_handle;
pub mod lexer;
pub mod parser;
pub mod prelude;
pub mod resolver;
pub mod semantic;
pub mod typeck;
pub mod visit;
