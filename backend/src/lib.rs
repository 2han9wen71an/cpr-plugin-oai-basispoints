//! OpenAI Basis Points 插件：复用宿主 Codex OAuth 凭据，经短路中间件代理 bps.openai.com。
//!
//! 业务处理器只通过公开 SDK 与宿主通信。

mod accounts;
mod app;
mod catalog;
pub mod config;
mod middleware;
pub mod protocol;
pub mod relay;
pub mod sse;
mod upstream;

pub use app::plugin;
pub use config::RuntimeConfig;

use gateway_plugin_sdk::Manifest;

pub const PLUGIN_ID: &str = "2han9wen71an.oai-basispoints";

/// 读取打包器和运行注册共同使用的作者清单。
///
/// # 错误
///
/// 作者清单无效时返回错误。
pub fn manifest() -> Result<Manifest, gateway_plugin_sdk::ManifestError> {
    Manifest::from_author_slice(include_bytes!("../../plugin.json"))
}
