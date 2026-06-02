//! Command implementations.
//!
//! Each `Command` variant in `cli.rs` calls one of these. The
//! per-subcommand logic lives in the submodules below; `cli.rs` keeps
//! calling `cmd::apply(...)` etc. through the re-exports here.
//!
//! NOTE: `owo_colors::OwoColorize` is intentionally NOT imported at
//! module scope — its blanket impl shadows inherent methods of
//! unrelated types (e.g. `ignore::WalkBuilder::hidden(bool)` collides
//! with `OwoColorize::hidden(&self)`). Each print function imports the
//! trait locally with `use owo_colors::OwoColorize as _;`.

mod absorb;
mod apply;
mod common;
mod diff;
mod doctor;
mod gc_backup;
mod hooks;
mod init;
mod link;
mod list;
mod render;
mod secret;
mod status;
mod unlink;
mod unmanaged;
mod update;

pub(crate) use absorb::*;
pub(crate) use apply::*;
pub(crate) use common::*;
pub(crate) use diff::*;
pub(crate) use doctor::*;
pub(crate) use gc_backup::*;
pub(crate) use hooks::*;
pub(crate) use init::*;
pub(crate) use link::*;
pub(crate) use list::*;
pub(crate) use render::*;
pub(crate) use secret::*;
pub(crate) use status::*;
pub(crate) use unlink::*;
pub(crate) use unmanaged::*;
pub(crate) use update::*;

#[cfg(test)]
mod tests;
