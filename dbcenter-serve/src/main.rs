//! DBCenter as a container workload.
//!
//! The engine is designed to be embedded in a process, with its durable copy on
//! object storage. That is also exactly the shape a scale-to-zero platform
//! wants: the machine is disposable, the bucket is not. This binary supplies the
//! lifecycle that makes the two fit together.
//!
//! On boot it restores every table it finds in the bucket, then starts
//! PostgreSQL on TCP so ordinary clients - psql, DBeaver, any driver - can
//! connect. While running it pushes pending changes periodically. On SIGTERM,
//! which is what a platform sends before stopping a machine, it pushes whatever
//! is left and shuts down cleanly, so nothing written since the last pass is
//! lost when the machine goes away.

mod config;
mod lifecycle;

use config::ServeConfig;
use std::process::ExitCode;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Used at image build time: download and unpack the PostgreSQL
    // distribution so a container starts without reaching Maven Central. A
    // machine that has to fetch 60MB before it can answer is not scale-to-zero.
    if std::env::args().any(|a| a == "--prefetch") {
        return match lifecycle::prefetch_binaries() {
            Ok(path) => {
                tracing::info!("PostgreSQL binaries ready at {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                tracing::error!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    let config = match ServeConfig::from_env() {
        Ok(c) => c,
        Err(message) => {
            tracing::error!("{}", message);
            return ExitCode::FAILURE;
        }
    };

    match lifecycle::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{}", e);
            ExitCode::FAILURE
        }
    }
}
