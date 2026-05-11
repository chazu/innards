use anyhow::Result;
use navsplat::inline_text::{Mode, run};

fn main() -> Result<()> {
    run(Mode::View)
}
