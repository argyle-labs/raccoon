//! raccoon — gaming-box diagnostics/repair plugin for orca.
//!
//! A **backend-only** cdylib: it contributes one `diagnostics` domain backend
//! (no `raccoon.` tool surface). Running on the gaming machine it diagnoses, it
//! inspects audio, CPU power mode, the scheduler, the GPU, and the shader cache,
//! and emits typed [`plugin_toolkit::contract::diagnostics::Finding`]s — each
//! with an optional repair the operator can run via `orca diagnostics repair`.
//!
//! The detection + remediation logic (formerly `doctor.sh`/`tune.sh`) lives in
//! [`checks`]; [`registration`] wires it to the diagnostics domain across the
//! FFI seam; [`abi_export`] is the cdylib entry point.

pub mod checks;
pub mod registration;

mod abi_export;

/// Registry name this plugin uses across the diagnostics domain.
pub const PROVIDER: &str = "raccoon";
