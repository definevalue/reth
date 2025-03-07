//! Utility functions.
use std::{
    env::VarError,
    path::{Path, PathBuf},
};
use walkdir::{DirEntry, WalkDir};

/// Utilities for parsing chainspecs
pub mod chainspec;

/// Finds all files in a directory with a given postfix.
pub(crate) fn find_all_files_with_postfix(path: &Path, postfix: &str) -> Vec<PathBuf> {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(postfix))
        .map(DirEntry::into_path)
        .collect::<Vec<PathBuf>>()
}

/// Parses a user-specified path with support for environment variables and common shorthands (e.g.
/// ~ for the user's home directory).
pub(crate) fn parse_path(value: &str) -> Result<PathBuf, shellexpand::LookupError<VarError>> {
    shellexpand::full(value).map(|path| PathBuf::from(path.into_owned()))
}

/// Tracing utility
pub mod reth_tracing {
    use tracing::Subscriber;
    use tracing_subscriber::{prelude::*, EnvFilter};

    /// Tracing modes
    pub enum TracingMode {
        /// Enable all traces.
        All,
        /// Enable debug traces.
        Debug,
        /// Enable info traces.
        Info,
        /// Enable warn traces.
        Warn,
        /// Enable error traces.
        Error,
        /// Disable tracing.
        Silent,
    }

    impl TracingMode {
        fn into_env_filter(self) -> EnvFilter {
            match self {
                Self::All => EnvFilter::new("reth=trace"),
                Self::Debug => EnvFilter::new("reth=debug"),
                Self::Info => EnvFilter::new("reth=info"),
                Self::Warn => EnvFilter::new("reth=warn"),
                Self::Error => EnvFilter::new("reth=error"),
                Self::Silent => EnvFilter::new(""),
            }
        }
    }

    impl From<u8> for TracingMode {
        fn from(value: u8) -> Self {
            match value {
                0 => Self::Error,
                1 => Self::Warn,
                2 => Self::Info,
                3 => Self::Debug,
                _ => Self::All,
            }
        }
    }

    /// Build subscriber
    // TODO: JSON/systemd support
    pub fn build_subscriber(mods: TracingMode) -> impl Subscriber {
        // TODO: Auto-detect
        let no_color = std::env::var("RUST_LOG_STYLE").map(|val| val == "never").unwrap_or(false);
        let with_target = std::env::var("RUST_LOG_TARGET").map(|val| val != "0").unwrap_or(false);

        // Take env over config
        let filter = if std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default().is_empty() {
            mods.into_env_filter()
        } else {
            EnvFilter::from_default_env()
        };

        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_ansi(!no_color).with_target(with_target))
            .with(filter)
    }
}
