//! Headless failover switch module.
//!
//! `src-tauri/src/proxy/failover_switch.rs` depends on Tauri (tray/events/state).
//! For service/headless runtime we keep the core switching behavior:
//! - de-dup concurrent switches
//! - update DB `is_current`
//! - update device-level settings
//! - update live backup (for `stop_with_restore`)

use crate::database::Database;
use crate::error::AppError;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct FailoverSwitchManager {
    pending_switches: Arc<RwLock<HashSet<String>>>,
    db: Arc<Database>,
}

impl FailoverSwitchManager {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            pending_switches: Arc::new(RwLock::new(HashSet::new())),
            db,
        }
    }

    pub async fn try_switch(
        &self,
        _app_handle: Option<&tauri::AppHandle>,
        app_type: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        let switch_key = format!("{app_type}:{provider_id}");

        {
            let mut pending = self.pending_switches.write().await;
            if pending.contains(&switch_key) {
                log::debug!("[Failover] 切换已在进行中，跳过: {app_type} -> {provider_id}");
                return Ok(false);
            }
            pending.insert(switch_key.clone());
        }

        let result = self
            .do_switch(app_type, provider_id, provider_name)
            .await;

        {
            let mut pending = self.pending_switches.write().await;
            pending.remove(&switch_key);
        }

        result
    }

    async fn do_switch(
        &self,
        app_type: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        // 只有启用代理接管的 app 才允许故障转移切换
        let app_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.enabled,
            Err(e) => {
                log::warn!("[FO-002] 无法读取 {app_type} 配置: {e}，跳过切换");
                return Ok(false);
            }
        };
        if !app_enabled {
            log::debug!("[Failover] {app_type} 未启用代理，跳过切换");
            return Ok(false);
        }

        log::info!("[FO-001] 切换: {app_type} → {provider_name}");

        // 1) 更新数据库 is_current
        self.db.set_current_provider(app_type, provider_id)?;

        // 2) 更新本地 settings（设备级）
        let app_type_enum = crate::app_config::AppType::from_str(app_type).map_err(|_| {
            AppError::Message(format!("无效的应用类型: {app_type}"))
        })?;
        crate::settings::set_current_provider(&app_type_enum, Some(provider_id))?;

        // 3) 更新 Live 备份（确保 stop_with_restore 能恢复到最新配置）
        if let Some(provider) = self.db.get_provider_by_id(provider_id, app_type)? {
            let backup_json = match app_type {
                "claude" | "codex" => serde_json::to_string(&provider.settings_config)
                    .map_err(|e| AppError::Config(format!("序列化 {app_type} 配置失败: {e}")))?,
                "gemini" => {
                    let env_backup = if let Some(env) = provider.settings_config.get("env") {
                        serde_json::json!({ "env": env })
                    } else {
                        serde_json::json!({ "env": {} })
                    };
                    serde_json::to_string(&env_backup)
                        .map_err(|e| AppError::Config(format!("序列化 {app_type} 配置失败: {e}")))?
                }
                _ => {
                    log::debug!("[Failover] 未知 app_type={app_type}，跳过备份更新");
                    return Ok(true);
                }
            };
            let _ = self.db.save_live_backup(app_type, &backup_json).await;
        }

        Ok(true)
    }
}
