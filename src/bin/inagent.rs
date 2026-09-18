use anyhow::Result;
use clap::Parser;
use crossterm::terminal;

#[derive(Parser)]
#[command(about = "Attach to a live agent transcript over a JSONL presentation protocol")]
struct Cli {
    /// Override the inline viewport height. By default inagent uses the lower
    /// half of the terminal, while retaining enough rows for its controls.
    #[arg(long)]
    height: Option<u16>,
}

fn lower_half_height(rows: u16) -> u16 {
    // The inline terminal anchors its viewport at the bottom. Give an odd-row
    // terminal's middle row to the lower half, then preserve inagent's minimum
    // header/transcript/composer/footer layout on very short terminals.
    rows.saturating_sub(rows / 2).max(11)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let height = cli.height.unwrap_or_else(|| {
        terminal::size()
            .map(|(_, rows)| lower_half_height(rows))
            .unwrap_or(24)
    });
    navsplat::agent::run(height)
}

#[cfg(test)]
mod tests {
    use super::lower_half_height;

    #[test]
    fn default_viewport_is_the_lower_half_with_a_layout_floor() {
        assert_eq!(lower_half_height(24), 12);
        assert_eq!(lower_half_height(25), 13);
        assert_eq!(lower_half_height(20), 11);
    }
}
