//! Domain backends for the backend-only export.
//!
//! - one `diagnostics` domain provider (`raccoon`) exposing two ops —
//!   `diagnose` and `repair`. The typed [`DiagnosticsProvider`] impl delegates
//!   to the detection + remediation logic in [`crate::checks`]; the toolkit's
//!   `diagnostics::dispatch_op` handles op routing and arg (de)coding.
//! - a `backup_kind` backend (`game-saves`). The builder has no typed backup
//!   facet, so it rides `.backend(def, dispatcher)` via [`backup_backend_def`] +
//!   [`backup_dispatcher`], which hands the bare op to the toolkit's
//!   `dispatch_kind_op` over [`GameSavesKind`].

use plugin_toolkit::abi::BackendDef;
use plugin_toolkit::backend_def::syncable_backup_kind_backend_def;
use plugin_toolkit::backup::dispatch_kind_op;
use plugin_toolkit::contract::BoxFuture;
use plugin_toolkit::contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome,
};

use plugin_toolkit::serde_json::Value;

use crate::game_saves::{GameSavesKind, KIND};

/// Bridge invoke-prefix for the `game-saves` backup KIND.
const BACKUP_PREFIX: &str = "raccoon.__backup_game_saves";

/// The `backup_kind` backend def for `game-saves`, opted into `backup.sync`.
pub fn backup_backend_def() -> BackendDef {
    syncable_backup_kind_backend_def(KIND, BACKUP_PREFIX)
}

/// Escape-hatch dispatcher for `raccoon.__backup_game_saves.*`. Returns `None`
/// for anything else so the builder falls through to the next dispatcher.
pub fn backup_dispatcher(tool: &str, args: Value) -> Option<Result<Value, Value>> {
    let op = tool
        .strip_prefix(BACKUP_PREFIX)
        .and_then(|s| s.strip_prefix('.'))?;
    Some(dispatch_kind_op(&GameSavesKind, op, args))
}

/// The diagnostics provider raccoon advertises.
pub struct RaccoonDiagnostics;

impl DiagnosticsProvider for RaccoonDiagnostics {
    fn name(&self) -> &str {
        crate::PROVIDER
    }

    fn diagnose(
        &self,
        args: DiagnoseArgs,
    ) -> BoxFuture<'_, plugin_toolkit::anyhow::Result<Vec<Finding>>> {
        Box::pin(async move { Ok(crate::checks::diagnose_typed(args)) })
    }

    fn repair(
        &self,
        args: RepairArgs,
    ) -> BoxFuture<'_, plugin_toolkit::anyhow::Result<RepairOutcome>> {
        Box::pin(async move { Ok(crate::checks::repair_typed(args)) })
    }
}
