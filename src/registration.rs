//! Domain-backend registration for the hybrid export.
//!
//! raccoon contributes one backend to orca's `contract` registries: a
//! `diagnostics` provider (`raccoon.__diag.<op>`) exposing two ops —
//! `diagnose` and `repair`. orca's loader installs a `DiagnosticsProxy` that
//! routes those two ops back through the FFI `invoke`; [`backend_dispatch`]
//! answers them.

use plugin_toolkit::abi::BackendDef;
use plugin_toolkit::serde_json;

const DIAG_PREFIX: &str = "raccoon.__diag";

/// The single backend descriptor this plugin advertises.
pub fn backends_json() -> String {
    let defs = vec![BackendDef {
        domain: "diagnostics".to_string(),
        name: crate::PROVIDER.to_string(),
        invoke_prefix: DIAG_PREFIX.to_string(),
        ..Default::default()
    }];
    serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string())
}

/// Answer `raccoon.__diag.{diagnose,repair}` calls the loader's proxy makes.
/// Returns `None` for any name this plugin doesn't own (there are no others).
pub fn backend_dispatch(name: &str, args_json: &str) -> Option<Result<String, String>> {
    let op = name.strip_prefix(DIAG_PREFIX)?.strip_prefix('.')?;
    Some(match op {
        "diagnose" => crate::checks::diagnose(args_json),
        "repair" => crate::checks::repair(args_json),
        other => Err(format!("raccoon: unknown diagnostics op '{other}'")),
    })
}
