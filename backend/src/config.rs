use serde_json::{Map, Value};

/// 安装配置的运行时投影；启动时从握手配置解析一次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub responses_url: String,
    pub models: Vec<ModelMapping>,
    pub catalog_target: String,
    pub max_response_bytes: usize,
    /// 允许代理 Basis Points 的账号（account id / email / 名称，大小写不敏感）。
    /// 空列表表示不限制，沿用宿主全部启用的 openai OAuth 账号。
    pub allowed_accounts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMapping {
    pub alias: String,
    pub upstream_model: String,
}

#[derive(Debug)]
pub struct InvalidConfig(pub String);

impl std::fmt::Display for InvalidConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "插件配置无效：{}", self.0)
    }
}

impl std::error::Error for InvalidConfig {}

impl RuntimeConfig {
    /// # 错误
    ///
    /// 配置缺失、类型不符或别名重复时返回人类可读原因。
    pub fn from_configuration(value: &Value) -> Result<Self, InvalidConfig> {
        let object = value
            .as_object()
            .ok_or_else(|| InvalidConfig("插件配置必须是 JSON 对象".to_owned()))?;
        let responses_url = string_field(object, "responsesUrl")?
            .ok_or_else(|| InvalidConfig("responsesUrl 不能为空".to_owned()))?;
        if !responses_url.starts_with("https://") {
            return Err(InvalidConfig("responsesUrl 必须是 https 地址".to_owned()));
        }
        let catalog_target =
            string_field(object, "catalogTarget")?.unwrap_or_else(|| "gpt-5.3-codex".to_owned());
        let max_response_bytes = match object.get("maxResponseBytes") {
            None | Some(Value::Null) => 64 * 1024 * 1024,
            Some(value) => value
                .as_u64()
                .filter(|size| (65_536..=134_217_728).contains(size))
                .ok_or_else(|| {
                    InvalidConfig("maxResponseBytes 必须在 64 KiB～128 MiB 之间".to_owned())
                })? as usize,
        };
        let models = match object.get("models") {
            None | Some(Value::Null) => {
                return Err(InvalidConfig("models 至少需要一条别名映射".to_owned()));
            }
            Some(Value::Array(items)) => {
                let mut models = Vec::with_capacity(items.len());
                for item in items {
                    let entry = item
                        .as_object()
                        .ok_or_else(|| InvalidConfig("models 条目必须是对象".to_owned()))?;
                    let alias = string_field(entry, "alias")?
                        .ok_or_else(|| InvalidConfig("alias 不能为空".to_owned()))?;
                    let upstream_model = string_field(entry, "upstreamModel")?
                        .ok_or_else(|| InvalidConfig("upstreamModel 不能为空".to_owned()))?;
                    models.push(ModelMapping {
                        alias,
                        upstream_model,
                    });
                }
                models
            }
            Some(_) => return Err(InvalidConfig("models 必须是数组".to_owned())),
        };
        if models.is_empty() {
            return Err(InvalidConfig("models 至少需要一条别名映射".to_owned()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for model in &models {
            if !seen.insert(model.alias.as_str()) {
                return Err(InvalidConfig(format!("模型别名重复：{}", model.alias)));
            }
            if seen.contains(model.upstream_model.as_str()) {
                return Err(InvalidConfig(format!(
                    "别名不能与上游模型同名：{}",
                    model.upstream_model
                )));
            }
        }
        let allowed_accounts = match object.get("allowedAccounts") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut accounts = Vec::with_capacity(items.len());
                for item in items {
                    let text = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            InvalidConfig("allowedAccounts 条目必须是非空字符串".to_owned())
                        })?;
                    accounts.push(text.to_ascii_lowercase());
                }
                accounts
            }
            Some(_) => return Err(InvalidConfig("allowedAccounts 必须是数组".to_owned())),
        };
        Ok(Self {
            responses_url,
            models,
            catalog_target,
            max_response_bytes,
            allowed_accounts,
        })
    }

    /// 检查指定账号是否在白名单中。白名单为空时全部允许。
    #[must_use]
    pub fn account_allowed(&self, account_id: &str, email: Option<&str>, name: &str) -> bool {
        if self.allowed_accounts.is_empty() {
            return true;
        }
        let id_lower = account_id.to_ascii_lowercase();
        let name_lower = name.to_ascii_lowercase();
        let email_lower = email.map(str::to_ascii_lowercase);
        self.allowed_accounts.iter().any(|pattern| {
            pattern == &id_lower
                || pattern == &name_lower
                || email_lower.as_deref().is_some_and(|e| e == pattern)
        })
    }

    /// 别名对应的 Basis Points 上游模型；非别名模型返回 `None`。
    #[must_use]
    pub fn resolve_upstream(&self, model: &str) -> Option<&str> {
        self.models
            .iter()
            .find(|entry| entry.alias == model)
            .map(|entry| entry.upstream_model.as_str())
    }

    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|entry| entry.alias.as_str())
    }
}

fn string_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, InvalidConfig> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Some(_) => Err(InvalidConfig(format!("{key} 必须是字符串"))),
    }
}
