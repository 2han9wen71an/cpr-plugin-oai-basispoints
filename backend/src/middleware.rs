use std::num::NonZeroUsize;
use std::sync::Arc;

use gateway_plugin_sdk::{
    PluginFault,
    call::middleware::{MiddlewareBodyFrame, MiddlewareBodyFraming, MiddlewareMount},
    client::{MiddlewareBody, MiddlewareCall, MiddlewareResponse},
};
use serde_json::{Value, json};

use crate::{
    accounts::AccountPicker,
    config::RuntimeConfig,
    protocol::{self, ProtocolError},
    sse::SseFrameSplitter,
    upstream,
};

pub struct Handler {
    pub config: Arc<RuntimeConfig>,
    pub picker: Arc<AccountPicker>,
}

/// 关键步骤经 host.log 留痕（事件名固定前缀，字段不含凭据与正文）。
async fn trace(host: &gateway_plugin_sdk::client::HostClient, step: &str, detail: &str) {
    let request = gateway_plugin_sdk::call::host::LogRequest {
        event: format!("oai-basispoints.{step}"),
        level: gateway_plugin_sdk::call::host::LogLevel::Info,
        fields: [("detail".to_owned(), json!(detail))]
            .into_iter()
            .collect(),
    };
    let reply = host.call("host.log", serde_json::to_value(&request).unwrap_or(json!({})), Vec::new());
    let _ = reply.await;
}

pub async fn handle(handler: Handler, call: MiddlewareCall) -> Result<MiddlewareResponse, PluginFault> {
    #[cfg(test)]
    eprintln!("[probe] handle entered, model={:?}", call.request.head.model);
    let Handler { config, picker } = handler;
    if call.request.head.mount != MiddlewareMount::Request {
        return call.next.run(call.request).await;
    }
    // 仅拦截声明别名模型的 /v1/responses 请求；其余原样交给宿主原生链路。
    let matched = call
        .request
        .head
        .model
        .as_deref()
        .zip(Some(call.request.head.endpoint.as_str()))
        .filter(|(_, endpoint)| *endpoint == "/v1/responses")
        .and_then(|(model, _)| config.resolve_upstream(model).map(str::to_owned));
    let Some(upstream_model) = matched else {
        #[cfg(test)]
        eprintln!("[probe] passthrough: before trace");
        trace(&call.host, "passthrough", "model not an alias").await;
        #[cfg(test)]
        eprintln!("[probe] passthrough: after trace, before next");
        let result = call.next.run(call.request).await;
        #[cfg(test)]
        eprintln!("[probe] passthrough: after next");
        return result;
    };
    let stream = call.request.head.transport
        == gateway_plugin_sdk::call::middleware::MiddlewareTransport::HttpSse;
    if !matches!(
        call.request.head.transport,
        gateway_plugin_sdk::call::middleware::MiddlewareTransport::HttpJson
            | gateway_plugin_sdk::call::middleware::MiddlewareTransport::HttpSse
    ) {
        // WS 等传输形态 v0.1 不支持短路，保持原生路径。
        return call.next.run(call.request).await;
    }
    if !call.request.head.body_visible {
        return Ok(error_response(
            500,
            "plugin_error",
            "请求正文未向插件投影，无法代理 Basis Points",
        ));
    }
    let source = match serde_json::from_slice::<Value>(&call.request.body) {
        Ok(Value::Object(source)) => source,
        Ok(_) => {
            return Ok(error_response(400, "invalid_request", "request body must be a JSON object"));
        }
        Err(_) => {
            return Ok(error_response(400, "invalid_request", "request body must be valid JSON"));
        }
    };
    trace(&call.host, "guard", "request body parsed").await;
    let prepared = match protocol::prepare_body(&source, &upstream_model, stream) {
        Ok(body) => body,
        Err(error) => {
            trace(&call.host, "guard_reject", error.code).await;
            return Ok(protocol_error_response(error));
        }
    };
    trace(
        &call.host,
        "prepared",
        if stream { "streaming" } else { "non-streaming" },
    )
    .await;
    proxy(config, picker, call, prepared, stream).await
}

async fn proxy(
    config: Arc<RuntimeConfig>,
    picker: Arc<AccountPicker>,
    call: MiddlewareCall,
    prepared: Vec<u8>,
    stream: bool,
) -> Result<MiddlewareResponse, PluginFault> {
    let credential = match picker.select(&call.host).await {
        Ok(credential) => credential,
        Err(message) => {
            trace(&call.host, "select_failed", &message).await;
            return Ok(error_response(503, "no_available_account", &message));
        }
    };
    trace(&call.host, "credential_ready", "oauth credential selected").await;
    let headers = upstream::auth_headers(
        &credential.access_token,
        &credential.chatgpt_account_id,
        stream,
    );
    let url = config.responses_url.clone();
    if !stream {
        let (response, body) = upstream::do_request(&call.host, &url, headers, prepared).await?;
        if !(200..300).contains(&response.status) {
            return Ok(upstream_error_response(response.status, &body));
        }
        if body.len() > config.max_response_bytes {
            return Ok(error_response(
                502,
                "upstream_response_too_large",
                "Basis Points response exceeds configured limit",
            ));
        }
        return Ok(MiddlewareResponse::direct(
            "openai",
            response.status,
            Vec::new(),
            MiddlewareBody::from_frames(
                MiddlewareBodyFraming::JsonDocument,
                vec![MiddlewareBodyFrame::new(body, true)],
            ),
        ));
    }
    let stream_response =
        match upstream::open_stream(&call.host, &url, headers, prepared).await {
            Ok(response) => response,
            Err(error) => {
                trace(&call.host, "open_stream_fault", "managed http stream failed").await;
                return Err(error);
            }
        };
    if !(200..300).contains(&stream_response.status) {
        trace(&call.host, "upstream_error", "non-2xx from basis points").await;
        let stream_id = stream_response.stream.clone().unwrap_or_default();
        let body = if stream_id.is_empty() {
            Vec::new()
        } else {
            upstream::read_full_stream(&call.host, &stream_id, config.max_response_bytes)
                .await
                .unwrap_or_default()
        };
        return Ok(upstream_error_response(stream_response.status, &body));
    }
    let stream_id = stream_response
        .stream
        .clone()
        .ok_or_else(|| upstream::fault("宿主未返回上游流句柄"))?;
    let (sender, body) =
        MiddlewareBody::channel(MiddlewareBodyFraming::SseEvent, NonZeroUsize::new(8).unwrap());
    let pump_host = call.host.clone();
    let maximum_bytes = config.max_response_bytes;
    trace(&call.host, "pump_started", "streaming response to client").await;
    tokio::spawn(async move {
        pump_stream(pump_host, stream_id, sender, maximum_bytes).await;
    });
    Ok(MiddlewareResponse::direct(
        "openai",
        stream_response.status,
        Vec::new(),
        body,
    ))
}

/// 上游 SSE → 中间件输出帧的泵任务；下游取消时宿主回调失败并结束泵。
async fn pump_stream(
    host: gateway_plugin_sdk::client::HostClient,
    stream_id: String,
    sender: gateway_plugin_sdk::client::MiddlewareBodySender,
    maximum_bytes: usize,
) {
    let mut splitter = SseFrameSplitter::new();
    let mut delivered = 0usize;
    let failure: Option<PluginFault> = loop {
        match upstream::read_stream_chunk(&host, &stream_id).await {
            Ok((eof, chunk)) => {
                delivered += chunk.len();
                if delivered > maximum_bytes {
                    break Some(upstream::fault("上游响应超过大小上限"));
                }
                for frame in splitter.feed(&chunk) {
                    if frame.done {
                        continue;
                    }
                    if sender.send(MiddlewareBodyFrame::new(frame.bytes, false)).await.is_err() {
                        // 下游已取消；关闭上游流并退出。
                        let _ = upstream::close_stream(&host, &stream_id).await;
                        return;
                    }
                }
                if eof {
                    let tail = splitter.finish();
                    if let Some(frame) = tail
                        && !frame.done
                        && sender
                            .send(MiddlewareBodyFrame::new(frame.bytes, false))
                            .await
                            .is_err()
                    {
                        let _ = upstream::close_stream(&host, &stream_id).await;
                        return;
                    }
                    break None;
                }
            }
            Err(error) => break Some(error),
        }
    };
    if let Some(error) = failure {
        upstream::trace_fault(&host, "pump_failed").await;
        let _ = sender.fail(error).await;
        let _ = upstream::close_stream(&host, &stream_id).await;
    } else {
        upstream::trace_fault(&host, "pump_completed").await;
    }
}

fn protocol_error_response(error: ProtocolError) -> MiddlewareResponse {
    error_response(error.status, error.code, &error.message)
}

fn upstream_error_response(status: u16, body: &[u8]) -> MiddlewareResponse {
    let mut message = String::from_utf8_lossy(body).to_string();
    // 保险起见对错误正文做令牌摘要；正常情况下正文不含凭据。
    message = redact(message);
    error_response(status, "upstream_error", &message)
}

fn redact(text: String) -> String {
    // 不在日志或错误正文中携带 JWT 形态的字符串。
    let mut result = String::with_capacity(text.len());
    let mut token = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '.' || character == '-' || character == '_' {
            token.push(character);
        } else {
            flush_token(&mut result, &mut token);
            result.push(character);
        }
    }
    flush_token(&mut result, &mut token);
    result
}

fn flush_token(result: &mut String, token: &mut String) {
    if token.split('.').count() == 3 && token.len() > 40 {
        result.push_str("[REDACTED]");
    } else {
        result.push_str(token);
    }
    token.clear();
}

pub fn error_response(status: u16, code: &str, message: &str) -> MiddlewareResponse {
    let body = json!({
        "error": {
            "type": "invalid_request_error",
            "code": code,
            "message": message,
        }
    })
    .to_string()
    .into_bytes();
    MiddlewareResponse::direct(
        "openai",
        status,
        Vec::new(),
        MiddlewareBody::from_frames(
            MiddlewareBodyFraming::RawBytes,
            vec![MiddlewareBodyFrame::new(body, true)],
        ),
    )
}
