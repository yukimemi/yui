//! Drift detection + auto-absorb decision logic.
//!
//! When a target was once linked but is now a separate inode (e.g. an editor
//! atomic-saved over the hardlink), classify the situation and decide how to
//! recover. See design doc for the truth table.

use camino::Utf8Path;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorbDecision {
    /// Inode broken but contents identical — relink only.
    RelinkOnly,
    /// target.mtime > source.mtime, contents differ → backup source, copy target → source, relink.
    AutoAbsorb,
    /// source.mtime ≥ target.mtime, contents differ → diff + ask (anomaly).
    NeedsConfirm,
    /// target equals source via current link mode — nothing to do.
    InSync,
    /// target missing → re-link from source.
    Restore,
}

pub fn classify(_source: &Utf8Path, _target: &Utf8Path) -> Result<AbsorbDecision> {
    todo!("absorb::classify")
}
