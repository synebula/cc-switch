use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode},
    response::{sse, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cc_switch_core::{
    app_config::AppType,
    claude_mcp,
    claude_plugin,
    database::Database,
    error::AppError,
    init_status::InitErrorPayload,
    provider::Provider,
    provider::UniversalProvider,
    services::{DiscoverableSkill, McpService, PromptService, ProviderService, SkillRepo, SkillService, SpeedtestService},
    settings::{AppSettings, CustomEndpoint},
    store::AppState,
};
use cc_switch_core::services::{
    env_checker::{check_env_conflicts as check_env_conflicts_service, EnvConflict},
    env_manager::{delete_env_vars as delete_env_vars_service, restore_from_backup, BackupInfo},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};

#[derive(Clone)]
struct ServerState {
    app_state: Arc<AppState>,
    auth_token: Option<String>,
    events: broadcast::Sender<ServerEvent>,
}

#[derive(Debug, Clone, Serialize)]
struct ServerEvent {
    event: String,
    payload: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyTestResult {
    success: bool,
    latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamProxyStatus {
    enabled: bool,
    #[serde(rename = "proxyUrl")]
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectedProxy {
    url: String,
    proxy_type: String,
    port: u16,
}

const PROXY_PORTS: &[(u16, &str, bool)] = &[
    (7890, "http", true),
    (7891, "socks5", false),
    (1080, "socks5", false),
    (8080, "http", false),
    (8888, "http", false),
    (3128, "http", false),
    (10808, "socks5", false),
    (10809, "http", false),
];

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct InvokeRequest {
    command: String,
    #[serde(default)]
    args: Value,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InvokeResponse<T: Serialize> {
    Ok { ok: bool, data: T },
    Err { ok: bool, error: String },
}

fn auth_ok(state: &ServerState, headers: &HeaderMap, query_token: Option<&str>) -> bool {
    let Some(expected) = state.auth_token.as_deref() else {
        return true;
    };
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let qt = query_token.unwrap_or("");
    bearer == expected || qt == expected
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    let body = Json(InvokeResponse::<Value>::Err {
        ok: false,
        error: msg.into(),
    });
    (status, body).into_response()
}

fn parse_proxy_host_port(url: &str) -> Option<(String, u16)> {
    let trimmed = url.trim();
    let scheme_pos = trimmed.find("://")?;
    let scheme = &trimmed[..scheme_pos].to_lowercase();
    let mut rest = &trimmed[scheme_pos + 3..];

    // Drop path/query/fragment
    for sep in ['/', '?', '#'] {
        if let Some(idx) = rest.find(sep) {
            rest = &rest[..idx];
        }
    }

    // Drop credentials
    if let Some(at) = rest.rfind('@') {
        rest = &rest[at + 1..];
    }

    let (host, port_str) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => (h, p),
        _ => {
            // No explicit port; use scheme defaults
            let default_port = match scheme.as_str() {
                "http" | "https" => 80,
                "socks5" | "socks5h" => 1080,
                _ => return None,
            };
            return Some((rest.to_string(), default_port));
        }
    };

    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

fn test_proxy_url_tcp(url: &str) -> ProxyTestResult {
    let start = Instant::now();
    let Some((host, port)) = parse_proxy_host_port(url) else {
        return ProxyTestResult {
            success: false,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some("Invalid proxy URL".to_string()),
        };
    };

    let target = format!("{host}:{port}");
    let timeout = Duration::from_millis(500);
    let result = target
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .and_then(|addr| TcpStream::connect_timeout(&addr, timeout).ok());

    match result {
        Some(_) => ProxyTestResult {
            success: true,
            latency_ms: start.elapsed().as_millis() as u64,
            error: None,
        },
        None => ProxyTestResult {
            success: false,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some("Connection failed".to_string()),
        },
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn events_sse(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    if !auth_ok(&state, &headers, q.token.as_deref()) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    let mut rx = state.events.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    let data = serde_json::to_string(&evt.payload).unwrap_or("null".to_string());
                    let msg = sse::Event::default().event(evt.event).data(data);
                    yield Ok::<_, std::convert::Infallible>(msg);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    sse::Sse::new(stream)
        .keep_alive(sse::KeepAlive::new().interval(Duration::from_secs(15)).text("ping"))
        .into_response()
}

async fn invoke(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(req): Json<InvokeRequest>,
) -> Response {
    if !auth_ok(&state, &headers, q.token.as_deref()) {
        return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    let command = req.command.trim().to_string();
    if command.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "missing command");
    }

    match handle_invoke(&state, &command, req.args).await {
        Ok(v) => Json(InvokeResponse::Ok { ok: true, data: v }).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e),
    }
}

async fn handle_invoke(state: &ServerState, command: &str, args: Value) -> Result<Value, String> {
    match command {
        // ====== app init ======
        "get_init_error" => {
            let payload: Option<InitErrorPayload> = cc_switch_core::init_status::get_init_error();
            Ok(serde_json::to_value(payload).map_err(|e| e.to_string())?)
        }
        "get_migration_result" => Ok(json!(false)),
        "get_skills_migration_result" => Ok(json!(null)),
        "is_portable_mode" => Ok(json!(false)),
        "get_tool_versions" => Ok(json!([])),

        // ====== env vars ======
        "check_env_conflicts" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let conflicts = check_env_conflicts_service(app)?;
            Ok(serde_json::to_value(conflicts).map_err(|e| e.to_string())?)
        }
        "delete_env_vars" => {
            let conflicts: Vec<EnvConflict> = serde_json::from_value(
                args.get("conflicts").cloned().unwrap_or(Value::Array(vec![])),
            )
            .map_err(|e| format!("invalid conflicts: {e}"))?;
            let info: BackupInfo = delete_env_vars_service(conflicts)?;
            Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
        }
        "restore_env_backup" => {
            let backup_path = args
                .get("backupPath")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if backup_path.trim().is_empty() {
                return Err("missing backupPath".to_string());
            }
            restore_from_backup(backup_path)?;
            Ok(json!(true))
        }

        // ====== settings ======
        "get_settings" => Ok(serde_json::to_value(cc_switch_core::settings::get_settings())
            .map_err(|e| e.to_string())?),
        "save_settings" => {
            let settings: AppSettings =
                serde_json::from_value(args.get("settings").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("invalid settings: {e}"))?;
            cc_switch_core::settings::update_settings(settings).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_claude_code_config_path" => Ok(json!(
            cc_switch_core::config::get_claude_settings_path()
                .to_string_lossy()
                .to_string()
        )),
        "get_config_dir" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let dir = match app_type {
                AppType::Claude => cc_switch_core::config::get_claude_config_dir(),
                AppType::Codex => cc_switch_core::codex_config::get_codex_config_dir(),
                AppType::Gemini => cc_switch_core::gemini_config::get_gemini_dir(),
                AppType::OpenCode => cc_switch_core::opencode_config::get_opencode_dir(),
            };
            Ok(json!(dir.to_string_lossy().to_string()))
        }
        "open_config_folder" | "open_app_config_folder" => Ok(json!(true)),
        "get_app_config_path" => Ok(json!(
            cc_switch_core::config::get_app_config_path()
                .to_string_lossy()
                .to_string()
        )),
        "get_claude_common_config_snippet" => Ok(json!(
            state
                .app_state
                .db
                .get_config_snippet("claude")
                .map_err(|e: AppError| e.to_string())?
        )),
        "set_claude_common_config_snippet" => {
            let snippet = args.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !snippet.trim().is_empty() {
                serde_json::from_str::<serde_json::Value>(&snippet)
                    .map_err(|e| format!("invalid json: {e}"))?;
            }
            let value = if snippet.trim().is_empty() { None } else { Some(snippet) };
            state
                .app_state
                .db
                .set_config_snippet("claude", value)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_common_config_snippet" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let value = state
                .app_state
                .db
                .get_config_snippet(app_type)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(value))
        }
        "set_common_config_snippet" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = args.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            if !snippet.trim().is_empty() {
                if app_type == "claude" || app_type == "gemini" {
                    serde_json::from_str::<serde_json::Value>(&snippet)
                        .map_err(|e| format!("invalid json: {e}"))?;
                }
            }
            let value = if snippet.trim().is_empty() { None } else { Some(snippet) };
            state
                .app_state
                .db
                .set_config_snippet(app_type, value)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "extract_common_config_snippet" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let app_enum = AppType::from_str(app_type).map_err(|e| e.to_string())?;
            let settings_config = args.get("settingsConfig").and_then(|v| v.as_str()).map(str::to_string);

            if let Some(settings_config) = settings_config.filter(|s| !s.trim().is_empty()) {
                let settings: serde_json::Value = serde_json::from_str(&settings_config)
                    .map_err(|e| format!("invalid json: {e}"))?;
                let snippet = ProviderService::extract_common_config_snippet_from_settings(app_enum, &settings)
                    .map_err(|e| e.to_string())?;
                return Ok(json!(snippet));
            }

            let snippet =
                ProviderService::extract_common_config_snippet(&state.app_state, app_enum).map_err(|e| e.to_string())?;
            Ok(json!(snippet))
        }
        "get_app_config_dir_override" => Ok(json!(
            cc_switch_core::app_store::get_app_config_dir_override()
                .map(|p| p.to_string_lossy().to_string())
        )),
        "set_app_config_dir_override" => Ok(json!(true)),
        "set_auto_launch" => Ok(json!(true)),
        "get_auto_launch_status" => Ok(json!(false)),
        "get_rectifier_config" => {
            let cfg = state
                .app_state
                .db
                .get_rectifier_config()
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "set_rectifier_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::types::RectifierConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid rectifier config: {e}"))?;
            state
                .app_state
                .db
                .set_rectifier_config(&cfg)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_log_config" => {
            let cfg = state
                .app_state
                .db
                .get_log_config()
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "set_log_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::types::LogConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid log config: {e}"))?;
            state
                .app_state
                .db
                .set_log_config(&cfg)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "sync_current_providers_live" => {
            ProviderService::sync_current_to_live(&state.app_state).map_err(|e| e.to_string())?;
            Ok(json!({ "success": true }))
        }
        "apply_claude_plugin_config" => {
            let official = args.get("official").and_then(|v| v.as_bool()).unwrap_or(false);
            let changed = if official {
                claude_plugin::clear_claude_config().map_err(|e| e.to_string())?
            } else {
                claude_plugin::write_claude_config().map_err(|e| e.to_string())?
            };
            Ok(json!(changed))
        }
        "apply_claude_onboarding_skip" => Ok(json!(
            claude_mcp::set_has_completed_onboarding().map_err(|e| e.to_string())?
        )),
        "clear_claude_onboarding_skip" => Ok(json!(
            claude_mcp::clear_has_completed_onboarding().map_err(|e| e.to_string())?
        )),

        // ====== providers ======
        "get_providers" => {
            let app = args
                .get("app")
                .and_then(|v| v.as_str())
                .unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let providers = ProviderService::list(&state.app_state, app_type).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(providers).map_err(|e| e.to_string())?)
        }
        "get_current_provider" => {
            let app = args
                .get("app")
                .and_then(|v| v.as_str())
                .unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let current = ProviderService::current(&state.app_state, app_type).map_err(|e| e.to_string())?;
            Ok(json!(current))
        }
        "add_provider" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let provider: Provider = serde_json::from_value(
                args.get("provider").cloned().ok_or("missing provider")?,
            )
            .map_err(|e| format!("invalid provider: {e}"))?;
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let ok = ProviderService::add(&state.app_state, app_type, provider).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "update_provider" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let provider: Provider = serde_json::from_value(
                args.get("provider").cloned().ok_or("missing provider")?,
            )
            .map_err(|e| format!("invalid provider: {e}"))?;
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let ok = ProviderService::update(&state.app_state, app_type, provider).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "delete_provider" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::delete(&state.app_state, app_type, id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "switch_provider" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::switch(&state.app_state, app_type, id).map_err(|e| e.to_string())?;
            let _ = state.events.send(ServerEvent {
                event: "provider-switched".to_string(),
                payload: json!({ "appType": app, "providerId": id }),
            });
            Ok(json!(true))
        }
        "import_default_config" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let ok =
                ProviderService::import_default_config(&state.app_state, app_type).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "remove_provider_from_live_config" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::remove_from_live_config(&state.app_state, app_type, id)
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "update_tray_menu" => Ok(json!(true)),
        "open_provider_terminal" => Ok(json!(true)),
        "import_opencode_providers_from_live" => {
            let count = cc_switch_core::services::provider::import_opencode_providers_from_live(&state.app_state)
                .map_err(|e| e.to_string())?;
            Ok(json!(count))
        }
        "get_opencode_live_provider_ids" => {
            let ids = cc_switch_core::opencode_config::get_providers()
                .map(|providers| providers.keys().cloned().collect::<Vec<_>>())
                .map_err(|e| e.to_string())?;
            Ok(json!(ids))
        }
        // ====== OMO (Oh-My-OpenCode) ======
        "read_omo_local_file" => {
            let data = cc_switch_core::services::OmoService::read_local_file().map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(data).map_err(|e| e.to_string())?)
        }
        "get_current_omo_provider_id" => {
            let provider = state
                .app_state
                .db
                .get_current_omo_provider("opencode")
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(provider.map(|p| p.id).unwrap_or_default()))
        }
        "get_omo_provider_count" => {
            let providers = state
                .app_state
                .db
                .get_all_providers("opencode")
                .map_err(|e: AppError| e.to_string())?;
            let count = providers
                .values()
                .filter(|p| p.category.as_deref() == Some("omo"))
                .count();
            Ok(json!(count))
        }
        "disable_current_omo" => {
            let providers = state
                .app_state
                .db
                .get_all_providers("opencode")
                .map_err(|e: AppError| e.to_string())?;
            for (id, p) in &providers {
                if p.category.as_deref() == Some("omo") {
                    state
                        .app_state
                        .db
                        .clear_omo_provider_current("opencode", id)
                        .map_err(|e: AppError| e.to_string())?;
                }
            }
            cc_switch_core::services::OmoService::delete_config_file().map_err(|e| e.to_string())?;
            Ok(json!(null))
        }
        "get_universal_providers" => {
            let providers = ProviderService::list_universal(&state.app_state).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(providers).map_err(|e| e.to_string())?)
        }
        "get_universal_provider" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let provider = ProviderService::get_universal(&state.app_state, id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(provider).map_err(|e| e.to_string())?)
        }
        "upsert_universal_provider" => {
            let provider: UniversalProvider = serde_json::from_value(
                args.get("provider").cloned().ok_or("missing provider")?,
            )
            .map_err(|e| format!("invalid provider: {e}"))?;
            let id = provider.id.clone();
            let ok = ProviderService::upsert_universal(&state.app_state, provider).map_err(|e| e.to_string())?;
            let _ = state.events.send(ServerEvent {
                event: "universal-provider-synced".to_string(),
                payload: json!({ "action": "upsert", "id": id }),
            });
            Ok(json!(ok))
        }
        "delete_universal_provider" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let ok = ProviderService::delete_universal(&state.app_state, id).map_err(|e| e.to_string())?;
            let _ = state.events.send(ServerEvent {
                event: "universal-provider-synced".to_string(),
                payload: json!({ "action": "delete", "id": id }),
            });
            Ok(json!(ok))
        }
        "sync_universal_provider" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let ok = ProviderService::sync_universal_to_apps(&state.app_state, id).map_err(|e| e.to_string())?;
            let _ = state.events.send(ServerEvent {
                event: "universal-provider-synced".to_string(),
                payload: json!({ "action": "sync", "id": id }),
            });
            Ok(json!(ok))
        }
        "update_providers_sort_order" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let updates: Vec<cc_switch_core::services::ProviderSortUpdate> = serde_json::from_value(
                args.get("updates").cloned().unwrap_or(Value::Array(vec![])),
            )
            .map_err(|e| format!("invalid updates: {e}"))?;
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let ok = ProviderService::update_sort_order(&state.app_state, app_type, updates)
                .map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "get_custom_endpoints" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let provider_id = args
                .get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if provider_id.is_empty() {
                return Err("missing providerId".to_string());
            }
            let eps: Vec<CustomEndpoint> =
                ProviderService::get_custom_endpoints(&state.app_state, app_type, provider_id)
                    .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(eps).map_err(|e| e.to_string())?)
        }
        "add_custom_endpoint" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let provider_id = args
                .get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if provider_id.is_empty() || url.trim().is_empty() {
                return Err("missing providerId/url".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::add_custom_endpoint(&state.app_state, app_type, provider_id, url)
                .map_err(|e| e.to_string())?;
            Ok(json!(null))
        }
        "remove_custom_endpoint" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let provider_id = args
                .get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if provider_id.is_empty() || url.trim().is_empty() {
                return Err("missing providerId/url".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::remove_custom_endpoint(&state.app_state, app_type, provider_id, url)
                .map_err(|e| e.to_string())?;
            Ok(json!(null))
        }
        "update_endpoint_last_used" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let provider_id = args
                .get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if provider_id.is_empty() || url.trim().is_empty() {
                return Err("missing providerId/url".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            ProviderService::update_endpoint_last_used(&state.app_state, app_type, provider_id, url)
                .map_err(|e| e.to_string())?;
            Ok(json!(null))
        }
        "queryProviderUsage" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            if provider_id.trim().is_empty() {
                return Err("missing providerId".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let result = ProviderService::query_usage(&state.app_state, app_type, provider_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(result).map_err(|e| e.to_string())?)
        }
        "testUsageScript" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let script_code = args.get("scriptCode").and_then(|v| v.as_str()).unwrap_or("");
            if provider_id.trim().is_empty() || script_code.trim().is_empty() {
                return Err("missing providerId/scriptCode".to_string());
            }
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(10);
            let api_key = args.get("apiKey").and_then(|v| v.as_str());
            let base_url = args.get("baseUrl").and_then(|v| v.as_str());
            let access_token = args.get("accessToken").and_then(|v| v.as_str());
            let user_id = args.get("userId").and_then(|v| v.as_str());
            let template_type = args.get("templateType").and_then(|v| v.as_str());

            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let result = ProviderService::test_usage_script(
                &state.app_state,
                app_type,
                provider_id,
                script_code,
                timeout,
                api_key,
                base_url,
                access_token,
                user_id,
                template_type,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(result).map_err(|e| e.to_string())?)
        }
        "read_live_provider_settings" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let v = ProviderService::read_live_settings(app_type).map_err(|e| e.to_string())?;
            Ok(v)
        }
        "test_api_endpoints" => {
            let urls: Vec<String> = serde_json::from_value(args.get("urls").cloned().unwrap_or(Value::Array(vec![])))
                .map_err(|e| format!("invalid urls: {e}"))?;
            let timeout = args.get("timeoutSecs").and_then(|v| v.as_u64());
            let result = SpeedtestService::test_endpoints(urls, timeout).await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(result).map_err(|e| e.to_string())?)
        }

        // ====== prompts ======
        "get_prompts" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let prompts = PromptService::get_prompts(&state.app_state, app_type).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(prompts).map_err(|e| e.to_string())?)
        }
        "upsert_prompt" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let prompt: cc_switch_core::prompt::Prompt = serde_json::from_value(
                args.get("prompt").cloned().ok_or("missing prompt")?,
            )
            .map_err(|e| format!("invalid prompt: {e}"))?;
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            PromptService::upsert_prompt(&state.app_state, app_type, id, prompt).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "delete_prompt" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            PromptService::delete_prompt(&state.app_state, app_type, id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "enable_prompt" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            PromptService::enable_prompt(&state.app_state, app_type, id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "import_prompt_from_file" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let id = PromptService::import_from_file(&state.app_state, app_type).map_err(|e| e.to_string())?;
            Ok(json!(id))
        }
        "get_current_prompt_file_content" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let content = PromptService::get_current_file_content(app_type).map_err(|e| e.to_string())?;
            Ok(json!(content))
        }

        // ====== usage stats ======
        "get_usage_summary" => {
            let start_date = args.get("startDate").and_then(|v| v.as_i64());
            let end_date = args.get("endDate").and_then(|v| v.as_i64());
            let summary = state
                .app_state
                .db
                .get_usage_summary(start_date, end_date)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(summary).map_err(|e| e.to_string())?)
        }
        "get_usage_trends" => {
            let start_date = args.get("startDate").and_then(|v| v.as_i64());
            let end_date = args.get("endDate").and_then(|v| v.as_i64());
            let trends = state
                .app_state
                .db
                .get_daily_trends(start_date, end_date)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(trends).map_err(|e| e.to_string())?)
        }
        "get_provider_stats" => {
            let stats = state
                .app_state
                .db
                .get_provider_stats()
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(stats).map_err(|e| e.to_string())?)
        }
        "get_model_stats" => {
            let stats = state
                .app_state
                .db
                .get_model_stats()
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(stats).map_err(|e| e.to_string())?)
        }
        "get_request_logs" => {
            let filters = serde_json::from_value::<cc_switch_core::services::usage_stats::LogFilters>(
                args.get("filters")
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new())),
            )
            .map_err(|e| format!("invalid filters: {e}"))?;
            let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let page_size = args.get("pageSize").and_then(|v| v.as_u64()).unwrap_or(20) as u32;
            let logs = state
                .app_state
                .db
                .get_request_logs(&filters, page, page_size)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(logs).map_err(|e| e.to_string())?)
        }
        "get_request_detail" => {
            let request_id = args.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
            if request_id.trim().is_empty() {
                return Err("missing requestId".to_string());
            }
            let detail = state
                .app_state
                .db
                .get_request_detail(request_id)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(detail).map_err(|e| e.to_string())?)
        }
        "get_model_pricing" => {
            let pricing = state.app_state.db.get_model_pricing().map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(pricing).map_err(|e| e.to_string())?)
        }
        "update_model_pricing" => {
            let model_id = args.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
            let display_name = args.get("displayName").and_then(|v| v.as_str()).unwrap_or("");
            let input_cost = args.get("inputCost").and_then(|v| v.as_str()).unwrap_or("");
            let output_cost = args.get("outputCost").and_then(|v| v.as_str()).unwrap_or("");
            let cache_read_cost = args.get("cacheReadCost").and_then(|v| v.as_str()).unwrap_or("");
            let cache_creation_cost = args.get("cacheCreationCost").and_then(|v| v.as_str()).unwrap_or("");
            if model_id.trim().is_empty() {
                return Err("missing modelId".to_string());
            }
            state
                .app_state
                .db
                .update_model_pricing(
                    model_id,
                    display_name,
                    input_cost,
                    output_cost,
                    cache_read_cost,
                    cache_creation_cost,
                )
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "delete_model_pricing" => {
            let model_id = args.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
            if model_id.trim().is_empty() {
                return Err("missing modelId".to_string());
            }
            state
                .app_state
                .db
                .delete_model_pricing(model_id)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "check_provider_limits" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if provider_id.trim().is_empty() || app_type.trim().is_empty() {
                return Err("missing providerId/appType".to_string());
            }
            let status = state
                .app_state
                .db
                .check_provider_limits(provider_id, app_type)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(status).map_err(|e| e.to_string())?)
        }

        // ====== stream check ======
        "stream_check_provider" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || provider_id.trim().is_empty() {
                return Err("missing appType/providerId".to_string());
            }
            let app_enum = AppType::from_str(app_type).map_err(|e| e.to_string())?;
            let config = state
                .app_state
                .db
                .get_stream_check_config()
                .map_err(|e: AppError| e.to_string())?;
            let providers = state
                .app_state
                .db
                .get_all_providers(app_enum.as_str())
                .map_err(|e: AppError| e.to_string())?;
            let provider = providers
                .get(provider_id)
                .ok_or_else(|| format!("provider {provider_id} not found"))?;
            let result = cc_switch_core::services::stream_check::StreamCheckService::check_with_retry(
                &app_enum,
                provider,
                &config,
            )
            .await
            .map_err(|e| e.to_string())?;
            let _ = state.app_state.db.save_stream_check_log(
                provider_id,
                &provider.name,
                app_enum.as_str(),
                &result,
            );
            Ok(serde_json::to_value(result).map_err(|e| e.to_string())?)
        }
        "stream_check_all_providers" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let proxy_targets_only = args.get("proxyTargetsOnly").and_then(|v| v.as_bool()).unwrap_or(false);
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let app_enum = AppType::from_str(app_type).map_err(|e| e.to_string())?;
            let config = state
                .app_state
                .db
                .get_stream_check_config()
                .map_err(|e: AppError| e.to_string())?;
            let providers = state
                .app_state
                .db
                .get_all_providers(app_enum.as_str())
                .map_err(|e: AppError| e.to_string())?;

            let allowed_ids: Option<std::collections::HashSet<String>> = if proxy_targets_only {
                let mut ids = std::collections::HashSet::new();
                if let Ok(Some(current_id)) = state.app_state.db.get_current_provider(app_enum.as_str()) {
                    ids.insert(current_id);
                }
                if let Ok(queue) = state.app_state.db.get_failover_queue(app_enum.as_str()) {
                    for item in queue {
                        ids.insert(item.provider_id);
                    }
                }
                Some(ids)
            } else {
                None
            };

            let mut results: Vec<(String, cc_switch_core::services::stream_check::StreamCheckResult)> = Vec::new();
            for (id, provider) in providers {
                if let Some(ids) = &allowed_ids {
                    if !ids.contains(&id) {
                        continue;
                    }
                }
                let result = cc_switch_core::services::stream_check::StreamCheckService::check_with_retry(
                    &app_enum,
                    &provider,
                    &config,
                )
                .await
                .unwrap_or_else(|e| cc_switch_core::services::stream_check::StreamCheckResult {
                    status: cc_switch_core::services::stream_check::HealthStatus::Failed,
                    success: false,
                    message: e.to_string(),
                    response_time_ms: None,
                    http_status: None,
                    model_used: String::new(),
                    tested_at: chrono::Utc::now().timestamp(),
                    retry_count: 0,
                });
                let _ = state.app_state.db.save_stream_check_log(
                    &id,
                    &provider.name,
                    app_enum.as_str(),
                    &result,
                );
                results.push((id, result));
            }
            Ok(serde_json::to_value(results).map_err(|e| e.to_string())?)
        }
        "get_stream_check_config" => {
            let cfg = state
                .app_state
                .db
                .get_stream_check_config()
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "save_stream_check_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::services::stream_check::StreamCheckConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid config: {e}"))?;
            state
                .app_state
                .db
                .save_stream_check_config(&cfg)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }

        // ====== sessions ======
        "list_sessions" => {
            let sessions = tokio::task::spawn_blocking(cc_switch_core::session_manager::scan_sessions)
                .await
                .map_err(|e| format!("Failed to scan sessions: {e}"))?;
            Ok(serde_json::to_value(sessions).map_err(|e| e.to_string())?)
        }
        "get_session_messages" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let source_path = args.get("sourcePath").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if provider_id.trim().is_empty() || source_path.trim().is_empty() {
                return Err("missing providerId/sourcePath".to_string());
            }
            let messages = tokio::task::spawn_blocking(move || {
                cc_switch_core::session_manager::load_messages(&provider_id, &source_path)
            })
            .await
            .map_err(|e| format!("Failed to load session messages: {e}"))?
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(messages).map_err(|e| e.to_string())?)
        }
        "launch_session_terminal" => Ok(json!(false)),

        // ====== deeplink ======
        "parse_deeplink" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.trim().is_empty() {
                return Err("missing url".to_string());
            }
            let req = cc_switch_core::deeplink::parse_deeplink_url(url).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(req).map_err(|e| e.to_string())?)
        }
        "merge_deeplink_config" => {
            let req: cc_switch_core::deeplink::DeepLinkImportRequest = serde_json::from_value(
                args.get("request").cloned().ok_or("missing request")?,
            )
            .map_err(|e| format!("invalid request: {e}"))?;
            let merged = cc_switch_core::deeplink::parse_and_merge_config(&req).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(merged).map_err(|e| e.to_string())?)
        }
        "import_from_deeplink_unified" => {
            let req: cc_switch_core::deeplink::DeepLinkImportRequest = serde_json::from_value(
                args.get("request").cloned().ok_or("missing request")?,
            )
            .map_err(|e| format!("invalid request: {e}"))?;

            match req.resource.as_str() {
                "provider" => {
                    let id = cc_switch_core::deeplink::import_provider_from_deeplink(&state.app_state, req)
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "type": "provider", "id": id }))
                }
                "prompt" => {
                    let id = cc_switch_core::deeplink::import_prompt_from_deeplink(&state.app_state, req)
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "type": "prompt", "id": id }))
                }
                "mcp" => {
                    let r = cc_switch_core::deeplink::import_mcp_from_deeplink(&state.app_state, req)
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "type": "mcp", "importedCount": r.imported_count, "importedIds": r.imported_ids, "failed": r.failed }))
                }
                "skill" => {
                    let key = cc_switch_core::deeplink::import_skill_from_deeplink(&state.app_state, req)
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "type": "skill", "key": key }))
                }
                other => Err(format!("Unsupported resource type: {other}")),
            }
        }

        // ====== proxy ======
        "is_proxy_running" => Ok(json!(state.app_state.proxy_service.is_running().await)),
        "start_proxy_server" => {
            state
                .app_state
                .proxy_service
                .start()
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "stop_proxy_with_restore" => {
            state
                .app_state
                .proxy_service
                .stop_with_restore()
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_proxy_takeover_status" => {
            let st = state
                .app_state
                .proxy_service
                .get_takeover_status()
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(st).map_err(|e| e.to_string())?)
        }
        "set_proxy_takeover_for_app" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            state
                .app_state
                .proxy_service
                .set_takeover_for_app(app_type, enabled)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "is_live_takeover_active" => Ok(json!(
            state
                .app_state
                .proxy_service
                .is_takeover_active()
                .await
                .map_err(|e| e.to_string())?
        )),
        "switch_proxy_provider" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || provider_id.trim().is_empty() {
                return Err("missing appType/providerId".to_string());
            }
            state
                .app_state
                .proxy_service
                .switch_proxy_target(app_type, provider_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_proxy_status" => {
            let st = state
                .app_state
                .proxy_service
                .get_status()
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(st).map_err(|e| e.to_string())?)
        }
        "get_proxy_config" => {
            let cfg = state
                .app_state
                .proxy_service
                .get_config()
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "update_proxy_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::ProxyConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid proxy config: {e}"))?;
            state
                .app_state
                .proxy_service
                .update_config(&cfg)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_global_proxy_config" => {
            let cfg = state
                .app_state
                .db
                .get_global_proxy_config()
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "update_global_proxy_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::GlobalProxyConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid global proxy config: {e}"))?;
            state
                .app_state
                .db
                .update_global_proxy_config(cfg)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_proxy_config_for_app" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let cfg = state
                .app_state
                .db
                .get_proxy_config_for_app(app)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "update_proxy_config_for_app" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::AppProxyConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid app proxy config: {e}"))?;
            state
                .app_state
                .db
                .update_proxy_config_for_app(cfg)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_default_cost_multiplier" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let v = state
                .app_state
                .db
                .get_default_cost_multiplier(app_type)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(v))
        }
        "set_default_cost_multiplier" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || value.trim().is_empty() {
                return Err("missing appType/value".to_string());
            }
            state
                .app_state
                .db
                .set_default_cost_multiplier(app_type, value)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_pricing_model_source" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let v = state
                .app_state
                .db
                .get_pricing_model_source(app_type)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(v))
        }
        "set_pricing_model_source" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || value.trim().is_empty() {
                return Err("missing appType/value".to_string());
            }
            state
                .app_state
                .db
                .set_pricing_model_source(app_type, value)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_provider_health" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if provider_id.trim().is_empty() || app_type.trim().is_empty() {
                return Err("missing providerId/appType".to_string());
            }
            let health = state
                .app_state
                .db
                .get_provider_health(provider_id, app_type)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(health).map_err(|e| e.to_string())?)
        }
        "reset_circuit_breaker" => {
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if provider_id.trim().is_empty() || app_type.trim().is_empty() {
                return Err("missing providerId/appType".to_string());
            }

            // 1) Reset DB health
            state
                .app_state
                .db
                .update_provider_health(provider_id, app_type, true, None)
                .await
                .map_err(|e: AppError| e.to_string())?;

            // 2) Reset in-memory circuit breaker (if proxy running)
            state
                .app_state
                .proxy_service
                .reset_provider_circuit_breaker(provider_id, app_type)
                .await
                .map_err(|e| e.to_string())?;

            // 3) If auto failover is enabled, consider switching back to higher priority provider
            let (app_enabled, auto_failover_enabled) = match state.app_state.db.get_proxy_config_for_app(app_type).await {
                Ok(cfg) => (cfg.enabled, cfg.auto_failover_enabled),
                Err(_) => (false, false),
            };
            if app_enabled && auto_failover_enabled && state.app_state.proxy_service.is_running().await {
                let current_id = state
                    .app_state
                    .db
                    .get_current_provider(app_type)
                    .map_err(|e: AppError| e.to_string())?;

                if let Some(current_id) = current_id {
                    let queue = state
                        .app_state
                        .db
                        .get_failover_queue(app_type)
                        .map_err(|e: AppError| e.to_string())?;

                    let restored_order = queue
                        .iter()
                        .find(|item| item.provider_id == provider_id)
                        .and_then(|item| item.sort_index);
                    let current_order = queue
                        .iter()
                        .find(|item| item.provider_id == current_id)
                        .and_then(|item| item.sort_index);

                    if let (Some(restored), Some(current)) = (restored_order, current_order) {
                        if restored < current {
                            state
                                .app_state
                                .proxy_service
                                .switch_proxy_target(app_type, provider_id)
                                .await
                                .map_err(|e| e.to_string())?;
                            let _ = state.events.send(ServerEvent {
                                event: "provider-switched".to_string(),
                                payload: json!({ "appType": app_type, "providerId": provider_id, "source": "circuitBreakerReset" }),
                            });
                        }
                    }
                }
            }

            Ok(json!(true))
        }
        "get_circuit_breaker_config" => {
            let cfg = state
                .app_state
                .db
                .get_circuit_breaker_config()
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(cfg).map_err(|e| e.to_string())?)
        }
        "update_circuit_breaker_config" => {
            let cfg = serde_json::from_value::<cc_switch_core::proxy::CircuitBreakerConfig>(
                args.get("config").cloned().ok_or("missing config")?,
            )
            .map_err(|e| format!("invalid config: {e}"))?;
            state
                .app_state
                .db
                .update_circuit_breaker_config(&cfg)
                .await
                .map_err(|e: AppError| e.to_string())?;
            state
                .app_state
                .proxy_service
                .update_circuit_breaker_configs(cfg)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_circuit_breaker_stats" => Ok(json!(null)),
        "get_failover_queue" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let queue = state
                .app_state
                .db
                .get_failover_queue(app_type)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(queue).map_err(|e| e.to_string())?)
        }
        "get_available_providers_for_failover" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let providers = state
                .app_state
                .db
                .get_available_providers_for_failover(app_type)
                .map_err(|e: AppError| e.to_string())?;
            Ok(serde_json::to_value(providers).map_err(|e| e.to_string())?)
        }
        "add_to_failover_queue" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || provider_id.trim().is_empty() {
                return Err("missing appType/providerId".to_string());
            }
            state
                .app_state
                .db
                .add_to_failover_queue(app_type, provider_id)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "remove_from_failover_queue" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let provider_id = args.get("providerId").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() || provider_id.trim().is_empty() {
                return Err("missing appType/providerId".to_string());
            }
            state
                .app_state
                .db
                .remove_from_failover_queue(app_type, provider_id)
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(true))
        }
        "get_auto_failover_enabled" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }
            let cfg = state
                .app_state
                .db
                .get_proxy_config_for_app(app_type)
                .await
                .map_err(|e: AppError| e.to_string())?;
            Ok(json!(cfg.auto_failover_enabled))
        }
        "set_auto_failover_enabled" => {
            let app_type = args.get("appType").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if app_type.trim().is_empty() {
                return Err("missing appType".to_string());
            }

            let p1_provider_id = if enabled {
                let mut queue = state
                    .app_state
                    .db
                    .get_failover_queue(app_type)
                    .map_err(|e: AppError| e.to_string())?;

                if queue.is_empty() {
                    let app_enum =
                        AppType::from_str(app_type).map_err(|_| format!("invalid appType: {app_type}"))?;
                    let current_id = cc_switch_core::settings::get_effective_current_provider(
                        &state.app_state.db,
                        &app_enum,
                    )
                    .map_err(|e| e.to_string())?;
                    let Some(current_id) = current_id else {
                        return Err("故障转移队列为空，且未设置当前供应商，无法开启故障转移".to_string());
                    };

                    state
                        .app_state
                        .db
                        .add_to_failover_queue(app_type, &current_id)
                        .map_err(|e: AppError| e.to_string())?;
                    queue = state
                        .app_state
                        .db
                        .get_failover_queue(app_type)
                        .map_err(|e: AppError| e.to_string())?;
                }

                queue
                    .first()
                    .map(|item| item.provider_id.clone())
                    .ok_or_else(|| "故障转移队列为空，无法开启故障转移".to_string())?
            } else {
                String::new()
            };

            let mut cfg = state
                .app_state
                .db
                .get_proxy_config_for_app(app_type)
                .await
                .map_err(|e: AppError| e.to_string())?;
            cfg.auto_failover_enabled = enabled;
            state
                .app_state
                .db
                .update_proxy_config_for_app(cfg)
                .await
                .map_err(|e: AppError| e.to_string())?;

            if enabled {
                state
                    .app_state
                    .proxy_service
                    .switch_proxy_target(app_type, &p1_provider_id)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = state.events.send(ServerEvent {
                    event: "provider-switched".to_string(),
                    payload: json!({ "appType": app_type, "providerId": p1_provider_id, "source": "failoverEnabled" }),
                });
            }

            Ok(json!(true))
        }

        // ====== upstream proxy ======
        "get_global_proxy_url" => Ok(json!(
            state
                .app_state
                .db
                .get_global_proxy_url()
                .map_err(|e: AppError| e.to_string())?
        )),
        "set_global_proxy_url" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let url_opt = if url.trim().is_empty() { None } else { Some(url.as_str()) };

            cc_switch_core::proxy::http_client::validate_proxy(url_opt).map_err(|e| e.to_string())?;
            state
                .app_state
                .db
                .set_global_proxy_url(url_opt)
                .map_err(|e: AppError| e.to_string())?;
            cc_switch_core::proxy::http_client::apply_proxy(url_opt).map_err(|e| e.to_string())?;

            Ok(json!(true))
        }
        "test_proxy_url" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.trim().is_empty() {
                return Err("missing url".to_string());
            }
            let result = test_proxy_url_tcp(url);
            Ok(serde_json::to_value(result).map_err(|e| e.to_string())?)
        }
        "get_upstream_proxy_status" => {
            let url = cc_switch_core::proxy::http_client::get_current_proxy_url();
            let st = UpstreamProxyStatus {
                enabled: url.is_some(),
                proxy_url: url,
            };
            Ok(serde_json::to_value(st).map_err(|e| e.to_string())?)
        }
        "scan_local_proxies" => {
            let found = tokio::task::spawn_blocking(|| {
                let mut found = Vec::<DetectedProxy>::new();
                for &(port, primary_type, is_mixed) in PROXY_PORTS {
                    let addr = format!("127.0.0.1:{port}");
                    let ok = addr
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut it| it.next())
                        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(100)).ok())
                        .is_some();
                    if ok {
                        found.push(DetectedProxy {
                            url: format!("{primary_type}://127.0.0.1:{port}"),
                            proxy_type: primary_type.to_string(),
                            port,
                        });
                        if is_mixed {
                            let alt_type = if primary_type == "http" { "socks5" } else { "http" };
                            found.push(DetectedProxy {
                                url: format!("{alt_type}://127.0.0.1:{port}"),
                                proxy_type: alt_type.to_string(),
                                port,
                            });
                        }
                    }
                }
                found
            })
            .await
            .unwrap_or_default();
            Ok(serde_json::to_value(found).map_err(|e| e.to_string())?)
        }

        // ====== mcp ======
        "get_claude_mcp_status" => {
            let st = claude_mcp::get_mcp_status().map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(st).map_err(|e| e.to_string())?)
        }
        "read_claude_mcp_config" => Ok(json!(
            claude_mcp::read_mcp_json().map_err(|e| e.to_string())?
        )),
        "upsert_claude_mcp_server" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let spec = args.get("spec").cloned().unwrap_or(Value::Null);
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let ok = claude_mcp::upsert_mcp_server(id, spec).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "delete_claude_mcp_server" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let ok = claude_mcp::delete_mcp_server(id).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "validate_mcp_command" => {
            let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.trim().is_empty() {
                return Err("missing cmd".to_string());
            }
            Ok(json!(
                claude_mcp::validate_command_in_path(cmd).map_err(|e| e.to_string())?
            ))
        }
        "get_mcp_config" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_ty = AppType::from_str(app).map_err(|e| e.to_string())?;
            let config_path = cc_switch_core::config::get_app_config_path()
                .to_string_lossy()
                .to_string();
            let all = McpService::get_all_servers(&state.app_state).map_err(|e| e.to_string())?;
            let mut servers = std::collections::HashMap::new();
            for (id, server) in all {
                if server.apps.is_enabled_for(&app_ty) {
                    servers.insert(id, server.server);
                }
            }
            Ok(json!({ "config_path": config_path, "servers": servers }))
        }
        "upsert_mcp_server_in_config" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let spec = args.get("spec").cloned().unwrap_or(Value::Null);
            let sync_other_side = args.get("sync_other_side").and_then(|v| v.as_bool()).unwrap_or(false);
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }

            let app_ty = AppType::from_str(app).map_err(|e| e.to_string())?;

            let existing_server = {
                let servers = state.app_state.db.get_all_mcp_servers().map_err(|e| e.to_string())?;
                servers.get(id).cloned()
            };

            let mut new_server = if let Some(mut existing) = existing_server {
                existing.server = spec.clone();
                existing.apps.set_enabled_for(&app_ty, true);
                existing
            } else {
                let mut apps = cc_switch_core::app_config::McpApps::default();
                apps.set_enabled_for(&app_ty, true);

                let name = spec
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(id)
                    .to_string();

                cc_switch_core::app_config::McpServer {
                    id: id.to_string(),
                    name,
                    server: spec,
                    apps,
                    description: None,
                    homepage: None,
                    docs: None,
                    tags: Vec::new(),
                }
            };

            if sync_other_side {
                new_server.apps.claude = true;
                new_server.apps.codex = true;
                new_server.apps.gemini = true;
                new_server.apps.opencode = true;
            }

            McpService::upsert_server(&state.app_state, new_server).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "delete_mcp_server_in_config" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            let ok = McpService::delete_server(&state.app_state, id).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "set_mcp_enabled" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if app.trim().is_empty() || id.trim().is_empty() {
                return Err("missing app/id".to_string());
            }
            let app_ty = AppType::from_str(app).map_err(|e| e.to_string())?;
            McpService::toggle_app(&state.app_state, id, app_ty, enabled).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "import_mcp_from_apps" => {
            let mut total = 0usize;
            total += McpService::import_from_claude(&state.app_state).unwrap_or(0);
            total += McpService::import_from_codex(&state.app_state).unwrap_or(0);
            total += McpService::import_from_gemini(&state.app_state).unwrap_or(0);
            total += McpService::import_from_opencode(&state.app_state).unwrap_or(0);
            Ok(json!(total))
        }
        "get_mcp_servers" => {
            let servers = McpService::get_all_servers(&state.app_state).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(servers).map_err(|e| e.to_string())?)
        }
        "upsert_mcp_server" => {
            let server: cc_switch_core::app_config::McpServer = serde_json::from_value(
                args.get("server").cloned().ok_or("missing server")?,
            )
            .map_err(|e| format!("invalid server: {e}"))?;
            McpService::upsert_server(&state.app_state, server).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "delete_mcp_server" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            let ok = McpService::delete_server(&state.app_state, id).map_err(|e| e.to_string())?;
            Ok(json!(ok))
        }
        "toggle_mcp_app" => {
            let server_id = args.get("serverId").and_then(|v| v.as_str()).unwrap_or("");
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if server_id.is_empty() || app.is_empty() {
                return Err("missing serverId/app".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            McpService::toggle_app(&state.app_state, server_id, app_type, enabled)
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }

        // ====== skills ======
        "get_installed_skills" => {
            let skills =
                SkillService::get_all_installed(&state.app_state.db).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "install_skill_unified" => {
            let skill: DiscoverableSkill = serde_json::from_value(
                args.get("skill").cloned().ok_or("missing skill")?,
            )
            .map_err(|e| format!("invalid skill: {e}"))?;
            let current_app = args.get("currentApp").and_then(|v| v.as_str()).unwrap_or("claude");
            let app_type = AppType::from_str(current_app).map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let installed = svc
                .install(&state.app_state.db, &skill, &app_type)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(installed).map_err(|e| e.to_string())?)
        }
        "uninstall_skill_unified" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.trim().is_empty() {
                return Err("missing id".to_string());
            }
            SkillService::uninstall(&state.app_state.db, id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "toggle_skill_app" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if id.trim().is_empty() || app.trim().is_empty() {
                return Err("missing id/app".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            SkillService::toggle_app(&state.app_state.db, id, &app_type, enabled)
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "scan_unmanaged_skills" => {
            let skills = SkillService::scan_unmanaged(&state.app_state.db).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "import_skills_from_apps" => {
            let directories: Vec<String> = serde_json::from_value(
                args.get("directories").cloned().unwrap_or(Value::Array(vec![])),
            )
            .map_err(|e| format!("invalid directories: {e}"))?;
            let skills =
                SkillService::import_from_apps(&state.app_state.db, directories).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "discover_available_skills" => {
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let skills = svc.discover_available(repos).await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "get_skills" => {
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let skills = svc
                .list_skills(repos, &state.app_state.db)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "get_skills_for_app" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let _ = AppType::from_str(app).map_err(|e| e.to_string())?;
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let skills = svc
                .list_skills(repos, &state.app_state.db)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(skills).map_err(|e| e.to_string())?)
        }
        "install_skill" => {
            let directory = args.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if directory.trim().is_empty() {
                return Err("missing directory".to_string());
            }
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let skills = svc.discover_available(repos).await.map_err(|e| e.to_string())?;
            let skill = skills
                .into_iter()
                .find(|s| {
                    let install_name = std::path::Path::new(&s.directory)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| s.directory.clone());
                    install_name.eq_ignore_ascii_case(&directory)
                        || s.directory.eq_ignore_ascii_case(&directory)
                })
                .ok_or_else(|| format!("Skill not found: {directory}"))?;
            let installed = svc
                .install(&state.app_state.db, &skill, &AppType::Claude)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(installed).map_err(|e| e.to_string())?)
        }
        "install_skill_for_app" => {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let directory = args.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if directory.trim().is_empty() {
                return Err("missing directory".to_string());
            }
            let app_type = AppType::from_str(app).map_err(|e| e.to_string())?;
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            let svc = SkillService::new();
            let skills = svc.discover_available(repos).await.map_err(|e| e.to_string())?;
            let skill = skills
                .into_iter()
                .find(|s| {
                    let install_name = std::path::Path::new(&s.directory)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| s.directory.clone());
                    install_name.eq_ignore_ascii_case(&directory)
                        || s.directory.eq_ignore_ascii_case(&directory)
                })
                .ok_or_else(|| format!("Skill not found: {directory}"))?;
            let installed = svc
                .install(&state.app_state.db, &skill, &app_type)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(installed).map_err(|e| e.to_string())?)
        }
        "uninstall_skill" => {
            let directory = args.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if directory.trim().is_empty() {
                return Err("missing directory".to_string());
            }
            let skills = SkillService::get_all_installed(&state.app_state.db).map_err(|e| e.to_string())?;
            let skill = skills
                .into_iter()
                .find(|s| s.directory.eq_ignore_ascii_case(&directory))
                .ok_or_else(|| format!("Skill not installed: {directory}"))?;
            SkillService::uninstall(&state.app_state.db, &skill.id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "uninstall_skill_for_app" => {
            let _app = args.get("app").and_then(|v| v.as_str()).unwrap_or("claude");
            let directory = args.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if directory.trim().is_empty() {
                return Err("missing directory".to_string());
            }
            let skills = SkillService::get_all_installed(&state.app_state.db).map_err(|e| e.to_string())?;
            let skill = skills
                .into_iter()
                .find(|s| s.directory.eq_ignore_ascii_case(&directory))
                .ok_or_else(|| format!("Skill not installed: {directory}"))?;
            SkillService::uninstall(&state.app_state.db, &skill.id).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "get_skill_repos" => {
            let repos = state.app_state.db.get_skill_repos().map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(repos).map_err(|e| e.to_string())?)
        }
        "add_skill_repo" => {
            let repo: SkillRepo = serde_json::from_value(
                args.get("repo").cloned().ok_or("missing repo")?,
            )
            .map_err(|e| format!("invalid repo: {e}"))?;
            state.app_state.db.save_skill_repo(&repo).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "remove_skill_repo" => {
            let owner = args.get("owner").and_then(|v| v.as_str()).unwrap_or("");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if owner.trim().is_empty() || name.trim().is_empty() {
                return Err("missing owner/name".to_string());
            }
            state.app_state.db.delete_skill_repo(owner, name).map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "install_skills_from_zip" => {
            let file_path = args.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
            let current_app = args.get("currentApp").and_then(|v| v.as_str()).unwrap_or("claude");
            if file_path.trim().is_empty() {
                return Err("missing filePath".to_string());
            }
            let app_type = AppType::from_str(current_app).map_err(|e| e.to_string())?;
            let installed = SkillService::install_from_zip(
                &state.app_state.db,
                std::path::Path::new(file_path),
                &app_type,
            )
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(installed).map_err(|e| e.to_string())?)
        }

        // ====== import/export helpers for web shim ======
        "get_snapshot" => {
            let sql = export_sql_to_string(state.app_state.db.clone())
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "format": "sql", "sql": sql }))
        }
        "apply_snapshot" => {
            let Value::Object(mut obj) = args else {
                return Err("missing snapshot".to_string());
            };
            let snapshot = obj.remove("snapshot").ok_or("missing snapshot")?;
            let Value::Object(mut snapshot) = snapshot else {
                return Err("invalid snapshot".to_string());
            };
            let Some(Value::String(sql)) = snapshot.remove("sql") else {
                return Err("snapshot.sql is required".to_string());
            };
            if sql.trim().is_empty() {
                return Err("snapshot.sql is required".to_string());
            }
            let backup_id = import_sql_from_string(state.app_state.db.clone(), sql)
                .await
                .map_err(|e| e.to_string())?;
            // 导入后同步 live + reload settings，复用上游逻辑
            let app_state = AppState::new(state.app_state.db.clone());
            if let Err(err) = ProviderService::sync_current_to_live(&app_state) {
                log::warn!("导入后同步 live 配置失败: {err}");
            }
            if let Err(err) = cc_switch_core::settings::reload_settings() {
                log::warn!("导入后重载设置失败: {err}");
            }
            Ok(json!({ "success": true, "message": "imported", "backupId": backup_id }))
        }

        // ====== SQL import/export (desktop commands parity) ======
        "export_config_to_file" => {
            let file_path = args.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
            if file_path.is_empty() {
                return Err("missing filePath".to_string());
            }
            let file_path_str = file_path.to_string();
            let file_path = PathBuf::from(file_path);
            let db = state.app_state.db.clone();
            tokio::task::spawn_blocking(move || db.export_sql(&file_path))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(json!({ "success": true, "message": "SQL exported successfully", "filePath": file_path_str }))
        }
        "import_config_from_file" => {
            let file_path = args.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
            if file_path.is_empty() {
                return Err("missing filePath".to_string());
            }
            let file_path_str = file_path.to_string();
            let file_path = PathBuf::from(file_path);
            let db = state.app_state.db.clone();
            let db2 = db.clone();
            let backup_id = tokio::task::spawn_blocking(move || db.import_sql(&file_path))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            // 导入后同步 live + reload settings
            let app_state = AppState::new(db2);
            if let Err(err) = ProviderService::sync_current_to_live(&app_state) {
                log::warn!("导入后同步 live 配置失败: {err}");
            }
            if let Err(err) = cc_switch_core::settings::reload_settings() {
                log::warn!("导入后重载设置失败: {err}");
            }
            Ok(json!({ "success": true, "message": "SQL imported successfully", "backupId": backup_id, "filePath": file_path_str }))
        }

        // ====== web-mode noops ======
        "open_external" | "check_for_updates" | "restart_app" | "set_window_theme" => Ok(json!(true)),
        "open_file_dialog" | "save_file_dialog" | "pick_directory" | "open_zip_file_dialog" => {
            Ok(json!(null))
        }

        other => {
            if other.starts_with("plugin:") {
                Ok(json!(true))
            } else {
                Err(format!("unsupported command: {other}"))
            }
        }
    }
}

async fn export_sql_to_string(db: Arc<Database>) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let tmp = tempfile::NamedTempFile::new()?;
        db.export_sql(tmp.path())?;
        let content = std::fs::read_to_string(tmp.path())?;
        Ok(content)
    })
    .await?
}

async fn import_sql_from_string(db: Arc<Database>, sql: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), sql.as_bytes())?;
        let backup_id = db.import_sql(tmp.path())?;
        Ok(backup_id)
    })
    .await?
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let listen = std::env::var("CC_SWITCH_WEB_LISTEN").unwrap_or_else(|_| "0.0.0.0:8787".to_string());
    let static_dir = std::env::var("CC_SWITCH_WEB_STATIC_DIR").unwrap_or_else(|_| "dist-web".to_string());
    let auth_token = std::env::var("CC_SWITCH_WEB_AUTH_TOKEN").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let allow_origin = std::env::var("CC_SWITCH_WEB_ALLOW_ORIGIN").unwrap_or_else(|_| "*".to_string());
    let auto_start_proxy = std::env::var("CC_SWITCH_AUTO_START_PROXY").ok().map(|v| v != "0").unwrap_or(true);
    let max_body_bytes: usize = std::env::var("CC_SWITCH_WEB_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(200 * 1024 * 1024);

    let db = Arc::new(Database::init()?);
    let app_state = Arc::new(AppState::new(db.clone()));

    if auto_start_proxy {
        // Headless 默认把代理绑定到 0.0.0.0，便于容器场景暴露端口；
        // 如需保持本机回环，仅在运行时通过更新 proxy 配置即可。
        if let Ok(mut global) = app_state.db.get_global_proxy_config().await {
            let override_addr = std::env::var("CC_SWITCH_PROXY_LISTEN_ADDRESS").ok();
            let override_port = std::env::var("CC_SWITCH_PROXY_LISTEN_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok());
            if let Some(addr) = override_addr.as_deref() {
                if !addr.trim().is_empty() {
                    global.listen_address = addr.trim().to_string();
                }
            } else if global.listen_address == "127.0.0.1" {
                global.listen_address = "0.0.0.0".to_string();
            }
            if let Some(p) = override_port {
                global.listen_port = p;
            }
            let _ = app_state.db.update_global_proxy_config(global).await;
        }

        match app_state.proxy_service.start().await {
            Ok(info) => log::info!("proxy started: {}:{}", info.address, info.port),
            Err(e) => log::warn!("proxy start failed: {e}"),
        }
    }

    let (tx, _rx) = broadcast::channel::<ServerEvent>(128);
    let state = ServerState {
        app_state,
        auth_token,
        events: tx,
    };

    let static_dir = PathBuf::from(static_dir);
    let cors = if allow_origin.trim() == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origin = allow_origin
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("*"));
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let router = Router::new()
        .route("/health", get(health))
        .route("/invoke", post(invoke))
        .route("/events", get(events_sse))
        .nest_service(
            "/",
            ServeDir::new(&static_dir).fallback(ServeFile::new(static_dir.join("index.html"))),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = listen.parse()?;
    log::info!("service listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
