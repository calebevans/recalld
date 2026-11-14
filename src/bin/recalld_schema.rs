//! Explicit offline canonical schema migration and attestation. No daemon/provider calls.
use clap::{Parser, Subcommand};
use recalld::storage::control::{Generation, GenerationRef};
use recalld::storage::schema_migration::{
    MigrationOptions, ProfileAttestationInput, inspect_schema, migrate_schema,
};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "recalld-schema",
    about = "Inspect or migrate a stopped store; schema-v2 destinations remain offline"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a stopped store and inspect its migration identity/bindings without changing it.
    Inspect { source: PathBuf },
    /// Migrate to a fresh offline destination. Never replaces or activates the source.
    Migrate {
        source: PathBuf,
        destination: PathBuf,
        /// Explicit migration timestamp, Unix milliseconds.
        #[arg(long, value_parser = parse_timestamp, allow_hyphen_values = true)]
        at: i64,
        /// Expected source lineage from `inspect`.
        #[arg(long, value_parser = parse_store_id)]
        expected_store_id: Uuid,
        /// Expected source generation: canonical decimal u63, from `inspect`.
        #[arg(long, value_parser = parse_generation)]
        expected_generation: Generation,
        /// JSON {namespaceId,namespaceName,descriptor}. Supplying this explicitly attests
        /// that ALL retained vectors in that namespace came from this exact profile.
        /// Repeat for multiple namespaces. Omit to preserve unknown legacy provenance.
        #[arg(long = "attest-profile", value_name = "FILE")]
        attest_profiles: Vec<PathBuf>,
    },
}

fn parse_generation(value: &str) -> Result<Generation, String> {
    Generation::try_from(value.to_owned()).map_err(|error| error.to_string())
}

fn parse_store_id(value: &str) -> Result<Uuid, &'static str> {
    let id = Uuid::parse_str(value).map_err(|_| "expected a nonnil store UUID")?;
    if id.is_nil() {
        return Err("expected a nonnil store UUID");
    }
    Ok(id)
}

fn parse_timestamp(value: &str) -> Result<i64, &'static str> {
    let time = value.parse().map_err(|_| "expected Unix milliseconds")?;
    chrono::DateTime::from_timestamp_millis(time).ok_or("unrepresentable migration timestamp")?;
    Ok(time)
}

fn run(args: Args) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    match args.command {
        Command::Inspect { source } => Ok(serde_json::to_value(inspect_schema(&source)?)?),
        Command::Migrate {
            source,
            destination,
            at,
            expected_store_id,
            expected_generation,
            attest_profiles,
        } => {
            // Parse all explicit inputs before acquiring storage or preparing staging.
            let mut attestations = Vec::with_capacity(attest_profiles.len());
            for path in attest_profiles {
                let bytes = std::fs::read(&path)?;
                let input: ProfileAttestationInput =
                    serde_json::from_slice(&bytes).map_err(|_| {
                        format!("invalid profile attestation JSON in {}", path.display())
                    })?;
                input
                    .descriptor
                    .validate()
                    .map_err(|_| format!("invalid profile descriptor in {}", path.display()))?;
                attestations.push(input);
            }
            let options = MigrationOptions {
                migration_now: at,
                expected_source: GenerationRef {
                    store_id: expected_store_id,
                    generation: expected_generation,
                },
                attestations,
            };
            let report = migrate_schema(&source, &destination, &options)?;
            eprintln!(
                "Destination remains offline. Previously dropped legacy labels cannot be recovered."
            );
            Ok(serde_json::to_value(report)?)
        }
    }
}

fn main() {
    match run(Args::parse()) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("Offline schema operation failed: {error}");
            std::process::exit(1);
        }
    }
}
