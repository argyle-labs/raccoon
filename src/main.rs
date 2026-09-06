//! Subprocess entrypoint for the raccoon plugin.
//!
//! A backend-only plugin: it advertises a single `diagnostics` domain provider
//! and owns no `raccoon.` tool surface. Running on the gaming machine it
//! diagnoses, it emits typed findings + repairs through orca's diagnostics
//! contract. The plugin is a `[[bin]]`, owns no runtime, and reaches orca only
//! through the socket.

plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    Plugin::named("raccoon")
        .version(env!("CARGO_PKG_VERSION"))
        .diagnostics(raccoon::registration::RaccoonDiagnostics)
        .serve()
}
