//! Dynamic (subprocess) entrypoint for the raccoon plugin.
//!
//! The toolkit's `serve_tool_plugin!` emits `fn main`, serving this plugin over
//! the orca socket. Dynamic replacement for the retired cdylib export — the
//! plugin is a `[[bin]]`, owns no runtime, and reaches orca only through the
//! socket.
//!
//! Hybrid arm: an (empty) `raccoon.` tool surface plus the `diagnostics` domain
//! backend. `target_compat` is empty — raccoon diagnoses whatever local gaming
//! box it runs on, so there's no external service version to gate against. The
//! backend descriptor comes from [`raccoon::registration::backends_json`] and
//! `raccoon.__diag.*` ops route through [`raccoon::registration::backend_dispatch`].

plugin_toolkit::serve_tool_plugin! {
    name: "raccoon",
    target_compat: "",
    backends: raccoon::registration::backends_json(),
    backend_dispatch: raccoon::registration::backend_dispatch,
}
