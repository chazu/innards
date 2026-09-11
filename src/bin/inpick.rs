use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use navsplat::picker::{Config, StaticProvider, read_json_lines, run_with, write_result_json};

#[derive(Debug, Parser)]
#[command(
    name = "inpick",
    about = "Select a structured candidate in an inline terminal picker"
)]
struct Cli {
    /// Root used to resolve relative preview paths.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, default_value_t = 20)]
    height: u16,

    #[arg(long, default_value = "inpick")]
    title: String,

    #[arg(long, default_value = "")]
    query: String,

    /// Return the highlighted candidate with this action when Ctrl-D is pressed.
    #[arg(long)]
    ctrl_d_action: Option<String>,

    /// Executable notified after a preview is displayed. Receives the candidate
    /// as JSON on stdin; may return an updated display object as JSON on stdout.
    #[arg(long)]
    preview_hook: Option<PathBuf>,

    /// Accepted for consistency with other Innards surfaces; output is always JSON.
    #[arg(long)]
    result_json: bool,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let candidates = read_json_lines(BufReader::new(std::io::stdin().lock()))?;
    let provider = StaticProvider::new(candidates);
    let mut config = Config::new(cli.root);
    config.height = cli.height;
    config.title = cli.title;
    config.initial_query = cli.query;
    config.ctrl_d_action = cli.ctrl_d_action;
    config.preview_hook = cli.preview_hook;
    let result = run_with(&provider, config)?;
    let _ = cli.result_json;
    write_result_json(&result)?;
    Ok(ExitCode::from(result.outcome.exit_code()))
}
