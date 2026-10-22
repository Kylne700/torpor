//! Find-break (spec 19): which turn broke it. Step 5 fills this in.
use crate::spec::Linted;

/// `promise add --repro`: not built yet (step 5).
pub fn add_repro(_l: &Linted, _background: bool, _replace: Option<i64>) -> i32 {
    crate::cli::err("Torpor cannot do --repro in this build yet.\n");
    2
}
