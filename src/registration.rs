//! Diagnostics provider for the backend-only export.
//!
//! raccoon contributes one `diagnostics` domain provider (`raccoon`) exposing
//! two ops — `diagnose` and `repair`. The typed [`DiagnosticsProvider`] impl
//! delegates to the detection + remediation logic in [`crate::checks`]; the
//! toolkit's `diagnostics::dispatch_op` handles op routing and arg (de)coding.

use plugin_toolkit::contract::BoxFuture;
use plugin_toolkit::contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome,
};

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
