use crate::relay;
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// 请求准备失败；直接映射为短路错误响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ProtocolError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

/// 请求发送给上游前的完整准备结果。
///
/// `body` 是可直接交给上游 HTTP 客户端的 JSON 字节；当请求包含工具目录
/// 或需要重放的工具历史时，`relay` 携带同一轮响应转换所需的上下文。
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
    pub relay: Option<relay::RelayContext>,
}

impl PreparedRequest {
    /// Borrows the encoded upstream request body.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Borrows the relay context, when this request uses tool relay.
    #[must_use]
    pub fn relay_context(&self) -> Option<&relay::RelayContext> {
        self.relay.as_ref()
    }

    /// Consumes the preparation result and returns its encoded body.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
}

/// Backwards-compatible name for callers that model the prepared payload as a body.
pub type PreparedBody = PreparedRequest;

fn relay_error(error: relay::RelayError) -> ProtocolError {
    ProtocolError::new(error.status, error.code, error.message)
}

fn relay_requested(source: &Map<String, Value>) -> bool {
    source.get("tools").is_some_and(|value| {
        !value.is_null() && value.as_array().is_none_or(|tools| !tools.is_empty())
    }) || source
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.as_object()
                    .and_then(|object| object.get("type"))
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        kind.eq_ignore_ascii_case("additional_tools")
                            || kind.eq_ignore_ascii_case("function_call")
                            || kind.eq_ignore_ascii_case("custom_tool_call")
                            || kind.eq_ignore_ascii_case("function_call_output")
                            || kind.eq_ignore_ascii_case("custom_tool_call_output")
                    })
            })
        })
        || source
            .get("tool_choice")
            .is_some_and(|value| !value.is_null())
        || source
            .get("parallel_tool_calls")
            .is_some_and(|value| !value.is_null())
}

/// Prepares a request body and, for relay requests, returns the context needed to
/// transform the matching upstream response.
pub fn prepare_request(
    source: &Map<String, Value>,
    upstream_model: &str,
    stream: bool,
) -> Result<PreparedRequest, ProtocolError> {
    if source
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(ProtocolError::new(
            400,
            "unsupported_continuation",
            "oai-basispoints does not support previous_response_id; omit it and send the complete input history",
        ));
    }
    match source.get("service_tier").and_then(Value::as_str) {
        None | Some("auto") | Some("default") => {}
        Some(_) => {
            return Err(ProtocolError::new(
                400,
                "unsupported_service_tier",
                "oai-basispoints supports only the standard service tier; omit service_tier or use auto/default",
            ));
        }
    }
    validate_text_format(source.get("text"))?;

    // The relay owns tool directory validation, developer prologue generation,
    // and history rewriting. Keep the legacy translator on an ordinary request
    // so no-tools requests retain their established wire shape byte-for-byte
    // (including the existing reference/reasoning filtering behavior).
    let (mut input_items, developer_instructions, relay_context) = if relay_requested(source) {
        let prepared_relay = relay::prepare_source(source).map_err(relay_error)?;
        let relay::PreparedRelay {
            input,
            developer_instructions,
            context,
        } = prepared_relay;
        (
            input,
            developer_instructions,
            context.is_active().then_some(context),
        )
    } else {
        (translate_input(source.get("input")), None, None)
    };

    // 会话指纹按序言注入前的翻译结果计算，与 CPA 插件一致。
    let history_fingerprint = conversation_fingerprint(&input_items);
    let mut prologue = Vec::with_capacity(2);
    if let Some(instructions) = source
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        prologue.push(message_item("developer", instructions));
    }
    if let Some(instructions) = developer_instructions {
        prologue.push(message_item("developer", &instructions));
    }
    if !prologue.is_empty() {
        prologue.extend(input_items);
        input_items = prologue;
    }

    let mut output = Map::new();
    output.insert("model".to_owned(), Value::String(upstream_model.to_owned()));
    output.insert(
        "model_selection".to_owned(),
        Value::String("explicit".to_owned()),
    );
    output.insert("stream".to_owned(), Value::Bool(stream));
    output.insert("store".to_owned(), Value::Bool(false));
    output.insert("input".to_owned(), Value::Array(input_items));
    output.insert(
        "reasoning_effort".to_owned(),
        Value::String(reasoning_effort(source)),
    );
    if let Some(policy) = source.get("context_management")
        && !policy.is_null()
    {
        match policy {
            Value::Array(entries) if entries.is_empty() => {}
            other => {
                output.insert("context_management".to_owned(), other.clone());
            }
        }
    }
    if let Some(key) = explicit_conversation_key(source) {
        output.insert("prompt_cache_key".to_owned(), Value::String(key.clone()));
    }
    let mut metadata = Map::new();
    if let Some(raw) = source.get("metadata").and_then(Value::as_object) {
        for (key, value) in raw {
            if matches!(key.as_str(), "turn_id" | "task_id" | "agent_iteration") {
                continue;
            }
            match value {
                Value::String(text) => {
                    metadata.insert(trim_key(key), Value::String(truncate(text, 512)));
                }
                Value::Number(_) | Value::Bool(_) => {
                    metadata.insert(
                        trim_key(key),
                        Value::String(truncate(&value.to_string(), 512)),
                    );
                }
                _ => {}
            }
        }
    }
    let conversation = explicit_conversation_key(source).unwrap_or(history_fingerprint);
    let (turn_fingerprint, iteration) = turn_state(source.get("input"));
    metadata.insert(
        "task_id".to_owned(),
        Value::String(uuid_v5(&format!("cpr-oai-basispoints/{conversation}"))),
    );
    metadata.insert(
        "turn_id".to_owned(),
        Value::String(uuid_v5(&format!(
            "cpr-oai-basispoints/{conversation}/turn/{turn_fingerprint}"
        ))),
    );
    metadata.insert(
        "agent_iteration".to_owned(),
        Value::String(iteration.to_string()),
    );
    output.insert("metadata".to_owned(), Value::Object(metadata));
    let body = serde_json::to_vec(&Value::Object(output))
        .map_err(|_| ProtocolError::new(500, "plugin_error", "请求编码失败"))?;
    Ok(PreparedRequest {
        body,
        relay: relay_context,
    })
}

/// Compatibility wrapper used by the current middleware and existing callers.
pub fn prepare_body(
    source: &Map<String, Value>,
    upstream_model: &str,
    stream: bool,
) -> Result<Vec<u8>, ProtocolError> {
    Ok(prepare_request(source, upstream_model, stream)?.body)
}

/// Alias for adapters that prefer an explicitly relay-aware name.
pub fn prepare_body_with_relay(
    source: &Map<String, Value>,
    upstream_model: &str,
    stream: bool,
) -> Result<PreparedRequest, ProtocolError> {
    prepare_request(source, upstream_model, stream)
}

fn validate_text_format(value: Option<&Value>) -> Result<(), ProtocolError> {
    let Some(text) = value else {
        return Ok(());
    };
    let Some(text) = text.as_object() else {
        return Err(ProtocolError::new(
            400,
            "invalid_text_config",
            "text must be an object",
        ));
    };
    let Some(format) = text.get("format") else {
        return Ok(());
    };
    let Some(format) = format.as_object() else {
        return Err(ProtocolError::new(
            400,
            "invalid_text_format",
            "text.format must be an object",
        ));
    };
    match format.get("type").and_then(Value::as_str) {
        Some("text") => {
            if format.len() != 1 {
                return Err(ProtocolError::new(
                    400,
                    "invalid_text_format",
                    "plain text.format accepts only type",
                ));
            }
            Ok(())
        }
        Some("json_object" | "json_schema") => Err(ProtocolError::new(
            400,
            "unsupported_text_format",
            "oai-basispoints does not implement structured text.format output; omit the format or use type=text",
        )),
        _ => Err(ProtocolError::new(
            400,
            "invalid_text_format",
            "text.format.type must be text, json_object, or json_schema",
        )),
    }
}

fn reasoning_effort(source: &Map<String, Value>) -> String {
    if let Some(reasoning) = source.get("reasoning").and_then(Value::as_object) {
        return normalize_effort(reasoning.get("effort"));
    }
    normalize_effort(source.get("reasoning_effort"))
}

fn normalize_effort(value: Option<&Value>) -> String {
    let text = value.and_then(Value::as_str).unwrap_or_default();
    let normalized = match text.trim().to_ascii_lowercase().as_str() {
        "x-high" | "extra-high" | "extra_high" | "max" => "xhigh".to_owned(),
        other => other.to_owned(),
    };
    if matches!(
        normalized.as_str(),
        "low" | "medium" | "high" | "xhigh" | "ultra"
    ) {
        return normalized;
    }
    "medium".to_owned()
}

/// 最小输入翻译：字符串包装为 user 消息，reasoning 仅保留密文条目，
/// 丢弃 item_reference／additional_tools，其余原样保留。
fn translate_input(raw: Option<&Value>) -> Vec<Value> {
    match raw {
        Some(Value::String(text)) => vec![message_item("user", text)],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let item_type = object
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                match item_type.as_str() {
                    "reasoning" => {
                        let encrypted = object
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        (!encrypted.is_empty()).then(|| {
                            serde_json::json!({
                                "type": "reasoning",
                                "summary": [],
                                "encrypted_content": encrypted,
                            })
                        })
                    }
                    "item_reference" | "additional_tools" => None,
                    _ => Some(item.clone()),
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn message_item(role: &str, text: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    serde_json::json!({
        "type": "message",
        "role": role,
        "content": [{"type": content_type, "text": text}],
    })
}

fn explicit_conversation_key(source: &Map<String, Value>) -> Option<String> {
    for key in [
        "prompt_cache_key",
        "promptCacheKey",
        "session_id",
        "sessionId",
    ] {
        if let Some(value) = source.get(key).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_owned());
        }
    }
    source
        .get("client_metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| {
            for key in ["session_id", "sessionId"] {
                if let Some(value) = metadata.get(key).and_then(Value::as_str)
                    && !value.trim().is_empty()
                {
                    return Some(value.trim().to_owned());
                }
            }
            None
        })
}

fn conversation_fingerprint(items: &[Value]) -> String {
    items
        .first()
        .map(|item| short_hash(item.to_string().as_bytes()))
        .unwrap_or_else(|| "anonymous".to_owned())
}

/// 会话内当前用户 turn 的稳定指纹与工具结果轮次；与 CPA 插件同构。
fn turn_state(raw: Option<&Value>) -> (String, u32) {
    let Some(Value::Array(items)) = raw else {
        return (
            match raw {
                Some(value) => short_hash(value.to_string().as_bytes()),
                None => short_hash(b"null"),
            },
            1,
        );
    };
    let mut last_user = None;
    for (index, value) in items.iter().enumerate() {
        if value
            .as_object()
            .and_then(|object| object.get("role"))
            .and_then(Value::as_str)
            .is_some_and(|role| role.eq_ignore_ascii_case("user"))
        {
            last_user = Some(index);
        }
    }
    let last_user = last_user.unwrap_or(0);
    let prefix = if items.is_empty() {
        short_hash(b"[]")
    } else {
        let end = (last_user + 1).min(items.len());
        short_hash(Value::Array(items[..end].to_vec()).to_string().as_bytes())
    };
    let mut iteration = 1;
    for value in items.get(last_user + 1..).unwrap_or(&[]) {
        if value
            .as_object()
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "function_call_output" | "custom_tool_call_output"))
        {
            iteration += 1;
        }
    }
    (prefix, iteration)
}

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

const UUID_V5_NAMESPACE: [u8; 16] = [
    0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
];

fn uuid_v5(name: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(UUID_V5_NAMESPACE);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = |slice: &[u8]| -> String { slice.iter().map(|byte| format!("{byte:02x}")).collect() };
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[0..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

fn trim_key(key: &str) -> String {
    key.chars().take(64).collect()
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
