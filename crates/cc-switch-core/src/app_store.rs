use std::path::PathBuf;

/// Headless 运行时不使用 Tauri store；保留同名 API 以复用 `src-tauri/src/config.rs`。
pub fn get_app_config_dir_override() -> Option<PathBuf> {
    None
}

