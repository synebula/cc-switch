//! MCP (Model Context Protocol) wrapper for headless runtime.
//!
//! Reuses `src-tauri/src/mcp/**` but keeps local fixes required for the
//! headless/core build (without desktop/Tauri runtime).

#[path = "../../../../src-tauri/src/mcp/claude.rs"]
mod claude;

#[path = "../../../../src-tauri/src/mcp/codex.rs"]
mod codex;

mod gemini;

#[path = "../../../../src-tauri/src/mcp/opencode.rs"]
mod opencode;

#[path = "../../../../src-tauri/src/mcp/validation.rs"]
mod validation;

pub use claude::{
    import_from_claude, remove_server_from_claude, sync_enabled_to_claude,
    sync_single_server_to_claude,
};
pub use codex::{
    import_from_codex, remove_server_from_codex, sync_enabled_to_codex, sync_single_server_to_codex,
};
pub use gemini::{
    import_from_gemini, remove_server_from_gemini, sync_enabled_to_gemini,
    sync_single_server_to_gemini,
};
pub use opencode::{
    import_from_opencode, remove_server_from_opencode, sync_single_server_to_opencode,
};

