use std::process::ExitCode;

use anyhow::Result;
use navsplat::inline_text::{Mode, run};

fn main() -> Result<ExitCode> {
    Ok(ExitCode::from(run(Mode::View)?))
}
