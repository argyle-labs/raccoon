//! raccoon — gaming-box diagnostics/repair plugin for orca.
//!
//! A **backend-only** plugin (no `raccoon.` tool surface) contributing:
//! - a `diagnostics` backend: running on the gaming machine it diagnoses, it
//!   inspects audio, CPU power mode, the scheduler, the GPU, and the shader
//!   cache, and emits typed [`plugin_toolkit::contract::diagnostics::Finding`]s —
//!   each with an optional repair the operator can run via `orca diagnostics repair`;
//! - a `game-saves` backup KIND ([`game_saves`]) capturing/restoring per-game
//!   saves, built on the host-independent primitives in [`saves`].
//!
//! The detection + remediation logic lives in [`checks`]; [`registration`] wires
//! both to their domains. The plugin is a `[[bin]]` that talks to the orca
//! daemon over its Unix socket (`src/main.rs`).

pub mod checks;
pub mod game_saves;
pub mod registration;
pub mod saves;

/// Registry name this plugin uses across the diagnostics domain.
pub const PROVIDER: &str = "raccoon";
