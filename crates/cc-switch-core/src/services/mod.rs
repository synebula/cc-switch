#[path = "../../../../src-tauri/src/services/config.rs"]
pub mod config;

#[path = "../../../../src-tauri/src/services/env_checker.rs"]
pub mod env_checker;

#[path = "../../../../src-tauri/src/services/env_manager.rs"]
pub mod env_manager;

#[path = "../../../../src-tauri/src/services/mcp.rs"]
pub mod mcp;

#[path = "../../../../src-tauri/src/services/omo.rs"]
pub mod omo;

#[path = "../../../../src-tauri/src/services/prompt.rs"]
pub mod prompt;

#[path = "../../../../src-tauri/src/services/provider/mod.rs"]
pub mod provider;

#[path = "../../../../src-tauri/src/services/proxy.rs"]
pub mod proxy;

#[path = "../../../../src-tauri/src/services/skill.rs"]
pub mod skill;

#[path = "../../../../src-tauri/src/services/speedtest.rs"]
pub mod speedtest;

#[path = "../../../../src-tauri/src/services/stream_check.rs"]
pub mod stream_check;

#[path = "../../../../src-tauri/src/services/usage_stats.rs"]
pub mod usage_stats;

pub use config::ConfigService;
pub use mcp::McpService;
pub use omo::OmoService;
pub use prompt::PromptService;
pub use provider::{ProviderService, ProviderSortUpdate};
pub use proxy::ProxyService;
pub use skill::{DiscoverableSkill, Skill, SkillRepo, SkillService, SyncMethod};
pub use speedtest::{EndpointLatency, SpeedtestService};
