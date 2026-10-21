pub mod capture;
pub mod cli;
pub mod hook;
pub mod mask;
pub mod merge;
pub mod restore;
pub mod retention;
pub mod scan;
pub mod status;
pub mod store;
pub mod transcript;
pub mod turn;
pub mod util;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
