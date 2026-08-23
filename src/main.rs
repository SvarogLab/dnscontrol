mod cli;
mod config;
mod diff;
mod gcp;
mod load;
mod model;
mod normalize;
mod plan;
mod run;
mod snippets;
mod watch;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::Cli::parse();

    // Set before the runtime exists, while the process is still single-threaded: set_var is only
    // sound when nothing else can be reading the environment. --credentials is a friendlier
    // spelling of the variable ADC already consults, so routing through it keeps credential
    // discovery in one place rather than reimplementing the dispatch on the key's `type` field.
    if let Some(path) = &args.credentials {
        unsafe { std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path) };
    }

    // `from_default_env()` alone defaults to ERROR, which would silence every info! and warn! -
    // including "deleting undeclared managed zone". A tool that destroys DNS must not do it quietly.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_ansi(false)
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "failed to start the async runtime"
            );
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run::run(&args)) {
        Ok(counts) => {
            // Watch mode logs each converge as it happens; this is the one-shot summary.
            if !args.watch {
                tracing::info!(
                    added = counts.added,
                    updated = counts.updated,
                    removed = counts.removed,
                    zones_created = counts.zones_created,
                    zones_deleted = counts.zones_deleted,
                    check = args.check,
                    "converge complete"
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "dnscontrol failed");
            ExitCode::FAILURE
        }
    }
}
