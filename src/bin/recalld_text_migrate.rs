//! Explicit offline migration. Does not start or contact the daemon.
use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    name = "recalld-text-migrate",
    about = "Copy a stopped legacy store into a fresh generation-text destination. Keep the source offline when cutting over."
)]
struct Args {
    source: PathBuf,
    destination: PathBuf,
}
fn main() {
    let args = Args::parse();
    match recalld::storage::text_migration::migrate_legacy_text(&args.source, &args.destination) {
        Ok(report) => println!(
            "{}",
            serde_json::to_string(&report).expect("serializable report")
        ),
        Err(error) => {
            eprintln!("Text migration failed: {error}");
            std::process::exit(1);
        }
    }
}
