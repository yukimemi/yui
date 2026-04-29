use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("template: {0}")]
    Template(String),

    #[error("git: {0}")]
    Git(String),

    #[error("source repo not found (set --source / $YUI_SOURCE)")]
    SourceNotFound,

    #[error("absorb conflict: {0}")]
    AbsorbConflict(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
