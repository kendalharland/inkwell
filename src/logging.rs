//! Layered logging setup.
//!
//! Two outputs: stderr (always) and an optional log file (when the config
//! specifies one). The non-blocking file writer returns a `WorkerGuard`
//! that MUST be kept alive for the process lifetime — dropping it flushes
//! and closes the sink. Hence the return value: callers stash it.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub fn init_logging(log_file: Option<&Path>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_target(false);

    if let Some(path) = log_file {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("warning: could not open log file {}: {}", path.display(), e);
                tracing_subscriber::registry()
                    .with(stderr_layer.with_filter(filter))
                    .init();
                return None;
            }
        };
        let (nb, guard) = tracing_appender::non_blocking(file);
        let file_layer = fmt::layer()
            .with_writer(nb)
            .with_ansi(false)
            .with_target(false);
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .with(filter)
            .init();
        Some(guard)
    } else {
        tracing_subscriber::registry()
            .with(stderr_layer.with_filter(filter))
            .init();
        None
    }
}
