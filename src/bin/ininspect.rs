use std::io::BufReader;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use navsplat::inspector::{Config, read_input, run_with, write_result_json};

#[derive(Debug, Parser)]
#[command(
    name = "ininspect",
    about = "Navigate structured object state and return explicit edit proposals"
)]
struct Cli {
    #[arg(long, default_value_t = 20)]
    height: u16,

    #[arg(long, default_value = "Object inspector")]
    title: String,

    /// Accepted for consistency with other Innards surfaces; output is always JSON.
    #[arg(long)]
    result_json: bool,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let input = read_input(BufReader::new(std::io::stdin().lock()))?;
    let config = Config {
        height: cli.height,
        title: cli.title,
    };
    let result = run_with(input, config)?;
    let _ = cli.result_json;
    write_result_json(&result)?;
    Ok(ExitCode::from(result.outcome.exit_code()))
}
