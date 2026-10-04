//! Subprocess entrypoint for the raccoon plugin.
//!
//! A backend-only plugin: it advertises a `diagnostics` domain provider and the
//! `game-saves` backup KIND, and owns no `raccoon.` tool surface. Running on the
//! gaming machine it diagnoses, it emits typed findings + repairs through orca's
//! diagnostics contract and backs up / restores that machine's game saves. The
//! plugin is a `[[bin]]`, owns no runtime, and reaches orca only through the
//! socket.

plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    Plugin::named("raccoon")
        .version(env!("CARGO_PKG_VERSION"))
        .diagnostics(raccoon::registration::RaccoonDiagnostics)
        .backend(
            raccoon::registration::backup_backend_def(),
            Box::new(raccoon::registration::backup_dispatcher),
        )
        .serve()
}
