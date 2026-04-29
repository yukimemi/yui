use anyhow::Result;
use clap::Parser;
use yui::cli::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    yui::init_tracing(cli.verbose);
    cli.run()
}
