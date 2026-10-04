//! Subprocess entrypoint for the raccoon plugin (see the crate docs for what
//! it contributes). A `[[bin]]` that owns no runtime and reaches orca only
//! through the socket.

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
