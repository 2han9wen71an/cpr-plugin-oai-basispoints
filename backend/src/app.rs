use std::sync::Arc;

use gateway_plugin_sdk::client::{AuthorError, ComposedPlugin, PluginBuilder};

use crate::{accounts::AccountPicker, catalog, config::RuntimeConfig, middleware};

/// 通过公开的类型化 SDK 组装处理器。
///
/// # 错误
///
/// 清单与处理器声明不一致时返回错误。
pub fn plugin(config: Arc<RuntimeConfig>) -> Result<ComposedPlugin, AuthorError> {
    let picker = Arc::new(AccountPicker::new());
    let middleware_config = Arc::clone(&config);
    let middleware_picker = Arc::clone(&picker);
    PluginBuilder::from_json(include_bytes!("../../plugin.json"))?
        .model_catalog(catalog::registration(&config))?
        .middleware(move |call| {
            let handler = middleware::Handler {
                config: Arc::clone(&middleware_config),
                picker: Arc::clone(&middleware_picker),
            };
            async move { middleware::handle(handler, call).await }
        })?
        .build()
}
