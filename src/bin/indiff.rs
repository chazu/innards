use std::io::Read;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use navsplat::review::{Config, run_with, write_result_json};

#[derive(Debug, Parser)]
#[command(
    name = "indiff",
    about = "Review a unified diff in an inline terminal without applying it"
)]
struct Cli {
    #[arg(long, default_value_t = 20)]
    height: u16,

    #[arg(long, default_value = "Review proposed changes")]
    title: String,

    /// Write one structured outcome record to stdout after terminal cleanup.
    #[arg(long)]
    result_json: bool,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let mut diff = String::new();
    std::io::stdin().read_to_string(&mut diff)?;
    let result = run_with(
        diff,
        Config {
            height: cli.height,
            title: cli.title,
        },
    )?;
    if cli.result_json {
        write_result_json(&result)?;
    }
    Ok(ExitCode::from(result.outcome.exit_code()))
}
