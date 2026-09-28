use gateway_plugin_sdk::call::catalog::{ModelAlias, ModelCatalogRegistration};

use crate::config::RuntimeConfig;

/// 目录注册：别名形式上指向宿主原生 openai 模型（目录合同要求目标已存在），
/// 实际请求由短路中间件接管，不会路由到该目标。目标默认值可用 catalogTarget 调整。
#[must_use]
pub fn registration(config: &RuntimeConfig) -> ModelCatalogRegistration {
    ModelCatalogRegistration {
        models: config
            .models
            .iter()
            .map(|model| {
                let target = if !model.upstream_model.trim().is_empty() {
                    model.upstream_model.trim().to_owned()
                } else {
                    config.catalog_target.clone()
                };
                ModelAlias {
                    id: model.alias.clone(),
                    provider: "openai".to_owned(),
                    model: target,
                }
            })
            .collect(),
    }
}
