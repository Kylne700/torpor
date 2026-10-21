pub mod capture;
pub mod hook;
pub mod mask;
pub mod retention;
pub mod scan;
pub mod store;
pub mod transcript;
pub mod turn;
pub mod util;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
