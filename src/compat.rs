//! The two places raccoon meets backup-seam features the current toolkit
//! doesn't have yet: an "unchanged since latest" backup outcome, and the
//! `syncable` opt-in on the kind's backend def. Each is a single function body
//! to switch over when the toolkit gains the field.

use plugin_toolkit::abi::BackendDef;
use plugin_toolkit::contract::backup::BackupOutcome;

/// The outcome for a backup whose merged state equals the latest one, or
/// `None` when the seam can't express "unchanged" — the caller then writes a
/// full payload as usual.
pub fn unchanged_outcome(_note: &str) -> Option<BackupOutcome> {
    None
}

/// Mark the `game-saves` backend def as opting into `backup.sync`.
pub fn syncable(def: BackendDef) -> BackendDef {
    def
}
