use anyhow::Result;
use clap::Parser;
use crossterm::terminal;
use std::io::Read;

#[derive(Parser)]
#[command(about = "Browse a Trashtalk source index without modifying it")]
struct Cli {
    #[arg(long)]
    height: Option<u16>,
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let height = cli.height.unwrap_or_else(|| {
        terminal::size()
            .map(|(_, h)| h.saturating_sub(h / 2).max(12))
            .unwrap_or(24)
    });
    navsplat::browser::run(&input, height)
}
