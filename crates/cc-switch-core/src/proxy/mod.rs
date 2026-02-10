//! Headless proxy module wrapper.
//!
//! Reuses `src-tauri/src/proxy/**` implementation but replaces the UI/tauri
//! dependent failover switch module with a headless version.

#[path = "../../../../src-tauri/src/proxy/body_filter.rs"]
pub mod body_filter;

#[path = "../../../../src-tauri/src/proxy/circuit_breaker.rs"]
pub mod circuit_breaker;

#[path = "../../../../src-tauri/src/proxy/error.rs"]
pub mod error;

#[path = "../../../../src-tauri/src/proxy/error_mapper.rs"]
pub mod error_mapper;

pub(crate) mod failover_switch;

#[path = "../../../../src-tauri/src/proxy/forwarder.rs"]
mod forwarder;

#[path = "../../../../src-tauri/src/proxy/handler_config.rs"]
pub mod handler_config;

#[path = "../../../../src-tauri/src/proxy/handler_context.rs"]
pub mod handler_context;

#[path = "../../../../src-tauri/src/proxy/handlers.rs"]
mod handlers;

#[path = "../../../../src-tauri/src/proxy/health.rs"]
mod health;

#[path = "../../../../src-tauri/src/proxy/http_client.rs"]
pub mod http_client;

#[path = "../../../../src-tauri/src/proxy/log_codes.rs"]
pub mod log_codes;

#[path = "../../../../src-tauri/src/proxy/model_mapper.rs"]
pub mod model_mapper;

#[path = "../../../../src-tauri/src/proxy/provider_router.rs"]
pub mod provider_router;

#[path = "../../../../src-tauri/src/proxy/providers/mod.rs"]
pub mod providers;

#[path = "../../../../src-tauri/src/proxy/response_handler.rs"]
pub mod response_handler;

#[path = "../../../../src-tauri/src/proxy/response_processor.rs"]
pub mod response_processor;

#[path = "../../../../src-tauri/src/proxy/server.rs"]
pub(crate) mod server;

#[path = "../../../../src-tauri/src/proxy/session.rs"]
pub mod session;

#[path = "../../../../src-tauri/src/proxy/thinking_rectifier.rs"]
pub mod thinking_rectifier;

#[path = "../../../../src-tauri/src/proxy/types.rs"]
pub mod types;

#[path = "../../../../src-tauri/src/proxy/usage/mod.rs"]
pub mod usage;

#[allow(unused_imports)]
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerStats, CircuitState,
};
#[allow(unused_imports)]
pub use error::ProxyError;
#[allow(unused_imports)]
pub use provider_router::ProviderRouter;
#[allow(unused_imports)]
pub use response_handler::{NonStreamHandler, ResponseType, StreamHandler};
#[allow(unused_imports)]
pub use session::{
    extract_session_id, ClientFormat, ProxySession, SessionIdResult, SessionIdSource,
};
#[allow(unused_imports)]
pub use types::{
    AppProxyConfig, GlobalProxyConfig, ProxyConfig, ProxyServerInfo, ProxyStatus, ProxyTakeoverStatus,
};
#[allow(unused_imports)]
pub(crate) use types::*;
