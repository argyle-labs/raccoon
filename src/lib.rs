//! raccoon — gaming-box diagnostics/repair plugin for orca.
//!
//! A **backend-only** plugin: it contributes one `diagnostics` domain backend
//! (no `raccoon.` tool surface). Running on the gaming machine it diagnoses, it
//! inspects audio, CPU power mode, the scheduler, the GPU, and the shader cache,
//! and emits typed [`plugin_toolkit::contract::diagnostics::Finding`]s — each
//! with an optional repair the operator can run via `orca diagnostics repair`.
//!
//! The detection + remediation logic lives in [`checks`]; [`registration`] wires
//! it to the diagnostics domain. The plugin is a `[[bin]]` that talks to the orca
//! daemon over its Unix socket (`src/main.rs`).

pub mod checks;
pub mod registration;

/// Registry name this plugin uses across the diagnostics domain.
pub const PROVIDER: &str = "raccoon";
