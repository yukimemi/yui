pub mod absorb;
pub mod backup;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod error;
pub mod icons;
pub mod link;
pub mod marker;
pub mod mount;
pub mod paths;
pub mod render;
pub mod status;
pub mod template;
pub mod vars;

pub use error::{Error, Result};

pub fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};
    let directive = match verbose {
        0 => "yui=info",
        1 => "yui=debug",
        _ => "yui=trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directive));
    fmt().with_env_filter(filter).with_target(false).init();
}
