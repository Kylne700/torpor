pub mod capture;
pub mod cli;
pub mod constraints;
pub mod deps;
pub mod destructive;
pub mod errkey;
pub mod findbreak;
pub mod gate;
pub mod history;
pub mod hook;
pub mod mask;
pub mod merge;
pub mod promise;
pub mod promise_cmd;
pub mod push;
pub mod rebuild;
pub mod restore;
pub mod runner;
pub mod retention;
pub mod rewind;
pub mod scan;
pub mod scratch;
pub mod shadow;
pub mod spec;
pub mod status;
pub mod store;
pub mod texts;
pub mod transcript;
pub mod turn;
pub mod util;
pub mod vault;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
