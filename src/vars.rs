//! Built-in `yui.*` variables exposed to Tera contexts.

use camino::Utf8Path;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct YuiVars {
    /// `"windows"` / `"macos"` / `"linux"` (from `std::env::consts::OS`).
    pub os: String,
    /// `"x86_64"` / `"aarch64"` (from `std::env::consts::ARCH`).
    pub arch: String,
    /// Machine hostname.
    pub host: String,
    /// Current user name.
    pub user: String,
    /// Absolute path to the dotfiles source repo (`$DOTFILES`).
    pub source: String,
}

impl YuiVars {
    pub fn detect(source: &Utf8Path) -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            host: whoami::hostname().unwrap_or_else(|_| "unknown".to_string()),
            user: whoami::username().unwrap_or_else(|_| "unknown".to_string()),
            source: source.to_string(),
        }
    }
}
