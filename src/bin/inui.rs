use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Retained inline widgets over a bounded, revisioned JSONL protocol")]
struct Cli {
    #[arg(long, default_value_t = 20)]
    height: u16,
    /// Write a bounded diagnostic summary on exit. Never uses protocol stdout.
    #[arg(long)]
    profile: Option<PathBuf>,
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    navsplat::ui::run(cli.height, cli.profile.as_deref())
}
