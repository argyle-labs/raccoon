//! ABI-stable cdylib export for the raccoon plugin.
//!
//! Backend-only: no `raccoon.` tool surface, just the `diagnostics` domain
//! backend (see [`crate::registration`]). `target_compat` is empty — raccoon
//! diagnoses whatever local gaming box it runs on, so there's no external
//! service version to gate against. The toolkit's [`export_tool_plugin!`] hybrid
//! arm generates the metadata fns + an `invoke` that routes `raccoon.__diag.*`
//! to [`crate::registration::backend_dispatch`]; the (empty) tool manifest
//! covers everything else.
//!
//! `abi_stable` remains a direct dep because `#[export_root_module]` (which the
//! macro invokes) expands to bare `::abi_stable` paths.

plugin_toolkit::export_tool_plugin! {
    name: "raccoon",
    target_compat: "",
    backends: crate::registration::backends_json(),
    backend_dispatch: crate::registration::backend_dispatch,
}
