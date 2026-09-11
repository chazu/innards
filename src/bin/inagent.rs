use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(about = "Attach to a live agent transcript over a JSONL presentation protocol")]
struct Cli {
    #[arg(long, default_value_t = 24)]
    height: u16,
}

fn main() -> Result<()> {
    navsplat::agent::run(Cli::parse().height)
}
