//! Binary entrypoint for the Recalld CLI.
//!
//! Parses command-line arguments, loads configuration from
//! `~/.recalld/config.toml`, applies CLI flag overrides, and
//! dispatches to the appropriate command handler.

use clap::Parser;
use recalld::cli::{self, Cli, CliConfig, HumanFormatter, JsonFormatter, OutputFormat, OutputFormatter};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mut config = match CliConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };
    config.apply_overrides(&cli);

    if let Err(e) = cli::run(cli, config.clone()).await {
        let format = config.default_format.clone();
        let formatter: Box<dyn OutputFormatter> = match format {
            OutputFormat::Json => Box::new(JsonFormatter),
            OutputFormat::Human => Box::new(HumanFormatter),
        };
        eprintln!("{}", formatter.error(&e));
        std::process::exit(1);
    }
}
