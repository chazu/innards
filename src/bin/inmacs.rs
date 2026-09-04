use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use navsplat::inline_text::{CliArgs, Mode, normalize_plus_line_args, run_with, write_result_json};

#[derive(Debug, Parser)]
#[command(name = "inmacs", about = "Inline terminal text editor")]
struct Cli {
    #[command(flatten)]
    editor: CliArgs,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse_from(normalize_plus_line_args(std::env::args_os()));
    let invocation = cli.editor.into_invocation(Mode::Edit)?;
    let result = run_with(invocation.config)?;
    if invocation.result_json {
        write_result_json(&result)?;
    }
    Ok(ExitCode::from(result.outcome.exit_code()))
}
