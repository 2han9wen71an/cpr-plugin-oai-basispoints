use std::num::NonZeroUsize;
use std::sync::Arc;

use gateway_plugin_sdk::{
    PluginFault,
    call::middleware::{MiddlewareBodyFrame, MiddlewareBodyFraming, MiddlewareMount},
    client::{CallCancellation, MiddlewareBody, MiddlewareCall, MiddlewareResponse},
};
use serde_json::{Map, Value, json};

use crate::{
    accounts::AccountPicker,
    config::RuntimeConfig,
    protocol::{self, PreparedRequest, ProtocolError},
    relay::{RelayContext, RelayError},
    sse::{self, SseFrameSplitter, SseJsonEvent, SseStreamTracker},
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
        fields: [("detail".to_owned(), json!(detail))].into_iter().collect(),
    };
    let reply = host.call(
        "host.log",
        serde_json::to_value(&request).unwrap_or(json!({})),
        Vec::new(),
    );
    let _ = reply.await;
}

pub async fn handle(
    handler: Handler,
    call: MiddlewareCall,
) -> Result<MiddlewareResponse, PluginFault> {
    #[cfg(test)]
    eprintln!(
        "[probe] handle entered, model={:?}",
        call.request.head.model
    );
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
            return Ok(error_response(
                400,
                "invalid_request",
                "request body must be a JSON object",
            ));
        }
        Err(_) => {
            return Ok(error_response(
                400,
                "invalid_request",
                "request body must be valid JSON",
            ));
        }
    };
    trace(&call.host, "guard", "request body parsed").await;
    let prepared = match protocol::prepare_request(&source, &upstream_model, stream) {
        Ok(prepared) => prepared,
        Err(error) => {
            trace(&call.host, "guard_reject", error.code).await;
            return Ok(protocol_error_response(error));
        }
    };
    trace(
        &call.host,
        "prepared",
        if stream {
            if prepared.relay_context().is_some() {
                "streaming with tool relay"
            } else {
                "streaming"
            }
        } else if prepared.relay_context().is_some() {
            "non-streaming with tool relay"
        } else {
            "non-streaming"
        },
    )
    .await;
    proxy(config, picker, call, prepared, stream).await
}

async fn proxy(
    config: Arc<RuntimeConfig>,
    picker: Arc<AccountPicker>,
    call: MiddlewareCall,
    prepared: PreparedRequest,
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
    let PreparedRequest {
        body: prepared_body,
        relay,
    } = prepared;
    let url = config.responses_url.clone();
    if !stream {
        let (response, body) =
            upstream::do_request(&call.host, &url, headers, prepared_body).await?;
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
        let body = if let Some(relay) = relay.as_ref() {
            match transform_json_response(relay, &body) {
                Ok(body) => body,
                Err(error) => return Ok(error_response(422, error.code, &error.message)),
            }
        } else {
            // Without a client relay catalog, preserve the upstream body byte
            // for byte even if it happens to contain tool-shaped JSON.
            body
        };
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
        match upstream::open_stream(&call.host, &url, headers, prepared_body).await {
            Ok(response) => response,
            Err(error) => {
                trace(
                    &call.host,
                    "open_stream_fault",
                    "managed http stream failed",
                )
                .await;
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
    let (sender, body) = MiddlewareBody::channel(
        MiddlewareBodyFraming::SseEvent,
        NonZeroUsize::new(8).unwrap(),
    );
    let pump_host = call.host.clone();
    let cancellation = call.cancellation.clone();
    let maximum_bytes = config.max_response_bytes;
    trace(&call.host, "pump_started", "streaming response to client").await;
    tokio::spawn(async move {
        pump_stream(
            pump_host,
            stream_id,
            sender,
            maximum_bytes,
            relay,
            cancellation,
        )
        .await;
    });
    Ok(MiddlewareResponse::direct(
        "openai",
        stream_response.status,
        Vec::new(),
        body,
    ))
}

/// Pump an upstream SSE stream into the middleware response body.
///
/// Relay tool events are held back until the terminal response snapshot is
/// available. This is intentionally a deterministic pre-commit boundary: text
/// and non-tool events still stream immediately, while a malformed native relay
/// can never leak a partial client tool call.
async fn pump_stream(
    host: gateway_plugin_sdk::client::HostClient,
    stream_id: String,
    sender: gateway_plugin_sdk::client::MiddlewareBodySender,
    maximum_bytes: usize,
    relay: Option<RelayContext>,
    cancellation: CallCancellation,
) {
    let active_relay = relay.is_some();
    let mut delivery = RelaySseDelivery::new(relay);
    let mut splitter = SseFrameSplitter::new();
    let mut delivered = 0usize;
    let mut stream_failure = None;

    loop {
        let read = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                drop(sender);
                let _ = upstream::close_stream(&host, &stream_id).await;
                return;
            }
            result = upstream::read_stream_chunk(&host, &stream_id) => result,
        };
        let (eof, chunk) = match read {
            Ok(value) => value,
            Err(error) => {
                stream_failure = Some(error);
                break;
            }
        };
        delivered = delivered.saturating_add(chunk.len());
        if delivered > maximum_bytes {
            stream_failure = Some(upstream::fault("上游响应超过大小上限"));
            break;
        }

        for frame in splitter.feed(&chunk) {
            let done = frame.done;
            match delivery.accept(frame.bytes, done) {
                Ok(frames) => {
                    if send_frames(&sender, frames).await.is_err() {
                        drop(sender);
                        let _ = upstream::close_stream(&host, &stream_id).await;
                        return;
                    }
                    if done {
                        let _ = send_terminal_done(&sender).await;
                        let _ = upstream::close_stream(&host, &stream_id).await;
                        drop(sender);
                        return;
                    }
                }
                Err(error) => {
                    if active_relay {
                        upstream::trace_fault(&host, relay_error_step(&error)).await;
                        let _ = send_relay_failure(&sender, delivery.response_id()).await;
                        let _ = send_terminal_done(&sender).await;
                    } else {
                        let _ = sender
                            .fail(relay_error_fault("invalid_sse", &error.message()))
                            .await;
                    }
                    upstream::trace_fault(&host, "relay_stream_failed").await;
                    drop(sender);
                    let _ = upstream::close_stream(&host, &stream_id).await;
                    return;
                }
            }
        }
        if eof {
            if let Some(frame) = splitter.finish() {
                let done = frame.done;
                match delivery.accept(frame.bytes, done) {
                    Ok(frames) => {
                        if send_frames(&sender, frames).await.is_err() {
                            drop(sender);
                            let _ = upstream::close_stream(&host, &stream_id).await;
                            return;
                        }
                        if done {
                            let _ = send_terminal_done(&sender).await;
                            let _ = upstream::close_stream(&host, &stream_id).await;
                            drop(sender);
                            return;
                        }
                    }
                    Err(error) => {
                        if active_relay {
                            let _ = send_relay_failure(&sender, delivery.response_id()).await;
                            let _ = send_terminal_done(&sender).await;
                        } else {
                            let _ = sender
                                .fail(relay_error_fault("invalid_sse", &error.message()))
                                .await;
                        }
                        upstream::trace_fault(&host, "relay_stream_failed").await;
                        drop(sender);
                        let _ = upstream::close_stream(&host, &stream_id).await;
                        return;
                    }
                }
            }
            if delivery.terminal_seen && !delivery.done_seen {
                let _ = send_terminal_done(&sender).await;
                let _ = upstream::close_stream(&host, &stream_id).await;
                drop(sender);
                return;
            }
            break;
        }
    }

    if let Some(error) = stream_failure {
        upstream::trace_fault(&host, "pump_failed").await;
        let _ = sender.fail(error).await;
        drop(sender);
        let _ = upstream::close_stream(&host, &stream_id).await;
        return;
    }
    match delivery.finish() {
        Ok(frames) => {
            if send_frames(&sender, frames).await.is_err() {
                drop(sender);
                let _ = upstream::close_stream(&host, &stream_id).await;
            } else {
                upstream::trace_fault(&host, "pump_completed").await;
            }
        }
        Err(error) => {
            if active_relay {
                let _ = send_relay_failure(&sender, delivery.response_id()).await;
                let _ = send_terminal_done(&sender).await;
            } else {
                let _ = sender
                    .fail(relay_error_fault("invalid_sse", &error.message()))
                    .await;
            }
            upstream::trace_fault(&host, "relay_terminal_invalid").await;
            drop(sender);
            let _ = upstream::close_stream(&host, &stream_id).await;
        }
    }
}

async fn send_terminal_done(
    sender: &gateway_plugin_sdk::client::MiddlewareBodySender,
) -> Result<(), gateway_plugin_sdk::client::SessionError> {
    sender
        .send(MiddlewareBodyFrame::new(b"data: [DONE]\n\n".to_vec(), true))
        .await
}

async fn send_frames(
    sender: &gateway_plugin_sdk::client::MiddlewareBodySender,
    frames: Vec<Vec<u8>>,
) -> Result<(), gateway_plugin_sdk::client::SessionError> {
    for bytes in frames {
        sender.send(MiddlewareBodyFrame::new(bytes, false)).await?;
    }
    Ok(())
}

/// Semantic SSE delivery state for one prepared relay request.
///
/// Native tool lifecycle events are retained only as a validation signal. The
/// terminal response snapshot is the source of truth for client-shaped events,
/// so a partial or malformed native call cannot reach the client.
struct RelaySseDelivery {
    context: Option<RelayContext>,
    active: bool,
    tool_events_seen: bool,
    terminal_seen: bool,
    done_seen: bool,
    response_id: Option<String>,
    tracker: SseStreamTracker,
}

impl RelaySseDelivery {
    fn new(context: Option<RelayContext>) -> Self {
        let active = context.is_some();
        Self {
            active,
            context,
            tool_events_seen: false,
            terminal_seen: false,
            done_seen: false,
            response_id: None,
            tracker: SseStreamTracker::new(),
        }
    }

    fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    fn accept(
        &mut self,
        bytes: Vec<u8>,
        done_frame: bool,
    ) -> Result<Vec<Vec<u8>>, RelayDeliveryError> {
        if done_frame {
            if self.active && !self.terminal_seen {
                return Err(RelayDeliveryError::MissingTerminal);
            }
            self.done_seen = true;
            return Ok(Vec::new());
        }
        if !self.active {
            return Ok(vec![bytes]);
        }
        if self.terminal_seen {
            return Err(RelayDeliveryError::EventsAfterTerminal);
        }

        let event = match sse::parse_sse_json_event(&bytes) {
            Ok(Some(event)) => event,
            Ok(None) => return Ok(vec![bytes]),
            Err(_) => {
                // Unknown/non-JSON provider frames remain opaque. They are not
                // interpreted as relay events and cannot manufacture a tool call.
                return Ok(vec![bytes]);
            }
        };
        self.tracker.observe_json(&event);
        if let Some(response_id) = event_response_id(&event) {
            self.response_id = Some(response_id);
        }

        let Some(event_type) = event.event_type() else {
            return Ok(vec![bytes]);
        };
        if is_native_tool_event(event_type, event.value()) {
            self.tool_events_seen = true;
            return Ok(Vec::new());
        }
        if !sse::is_terminal_event(event_type) {
            return Ok(vec![bytes]);
        }

        self.terminal_seen = true;
        validate_terminal_event(event_type, &event)?;
        if matches!(
            event_type,
            "response.failed" | "error" | "response.cancelled" | "response.canceled"
        ) {
            // Upstream failure is already a complete response. Never replay a
            // partial native call after a failed response.
            self.tool_events_seen = false;
            return Ok(vec![bytes]);
        }
        if !matches!(
            event_type,
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            return Ok(vec![bytes]);
        }

        let response = event.value().get("response").unwrap_or(event.value());
        if !response.is_object() {
            return Err(RelayDeliveryError::InvalidTerminal);
        }
        let transformed = self
            .context
            .as_ref()
            .ok_or(RelayDeliveryError::InvalidTerminal)?
            .transform_response(response)
            .map_err(RelayDeliveryError::Relay)?;
        if !transformed.changed {
            if self.tool_events_seen {
                return Err(RelayDeliveryError::ToolEventsWithoutTransformedCall);
            }
            return Ok(vec![bytes]);
        }

        let transformed_response = transformed.response;
        let terminal = if event.value().get("response").is_some() {
            let mut envelope = event
                .value()
                .as_object()
                .cloned()
                .ok_or(RelayDeliveryError::InvalidTerminal)?;
            envelope.insert("response".to_owned(), transformed_response.clone());
            Value::Object(envelope)
        } else {
            transformed_response.clone()
        };
        let response_id = transformed_response
            .get("id")
            .and_then(Value::as_str)
            .or(self.response_id.as_deref());
        let mut output = client_tool_event_frames(&transformed_response, response_id);
        output.push(encode_sse_frame(event_type, &terminal));
        self.tool_events_seen = false;
        Ok(output)
    }

    fn finish(&self) -> Result<Vec<Vec<u8>>, RelayDeliveryError> {
        if self.active && (!self.terminal_seen || !self.done_seen) {
            return Err(RelayDeliveryError::MissingTerminal);
        }
        Ok(Vec::new())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RelayDeliveryError {
    Relay(RelayError),
    InvalidTerminal,
    MissingTerminal,
    ToolEventsWithoutTransformedCall,
    EventsAfterTerminal,
}

impl RelayDeliveryError {
    fn message(&self) -> String {
        match self {
            Self::Relay(error) => error.message.clone(),
            Self::InvalidTerminal => {
                "Basis Points returned an invalid response terminal".to_owned()
            }
            Self::MissingTerminal => {
                "Basis Points response stream ended without a terminal event".to_owned()
            }
            Self::ToolEventsWithoutTransformedCall => {
                "Basis Points returned tool events without a valid client tool call".to_owned()
            }
            Self::EventsAfterTerminal => {
                "Basis Points returned events after the terminal event".to_owned()
            }
        }
    }
}

fn relay_error_step(error: &RelayDeliveryError) -> &'static str {
    match error {
        RelayDeliveryError::Relay(_) => "relay_invalid_tool_call",
        RelayDeliveryError::InvalidTerminal => "relay_invalid_terminal",
        RelayDeliveryError::MissingTerminal => "relay_missing_terminal",
        RelayDeliveryError::ToolEventsWithoutTransformedCall => "relay_tool_without_call",
        RelayDeliveryError::EventsAfterTerminal => "relay_events_after_terminal",
    }
}

fn is_native_tool_event(event_type: &str, value: &Value) -> bool {
    match event_type {
        "response.output_item.added" | "response.output_item.done" => value
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "function_call" | "custom_tool_call")),
        "response.function_call_arguments.delta"
        | "response.function_call_arguments.done"
        | "response.custom_tool_call_input.delta"
        | "response.custom_tool_call_input.done" => true,
        _ => false,
    }
}

fn event_response_id(event: &SseJsonEvent) -> Option<String> {
    event
        .value()
        .pointer("/response/id")
        .and_then(Value::as_str)
        .or_else(|| event.value().get("response_id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn validate_terminal_event(
    event_type: &str,
    event: &SseJsonEvent,
) -> Result<(), RelayDeliveryError> {
    let status = sse::sse_event_status(event);
    let valid = match event_type {
        "response.completed" => status.is_none_or(|status| status == "completed"),
        "response.incomplete" => status.is_none_or(|status| status == "incomplete"),
        "response.failed" | "error" => status.is_none_or(|status| status == "failed"),
        "response.cancelled" | "response.canceled" => {
            status.is_none_or(|status| matches!(status, "cancelled" | "canceled"))
        }
        "response.done" => true,
        _ => true,
    };
    valid
        .then_some(())
        .ok_or(RelayDeliveryError::InvalidTerminal)
}

fn client_tool_event_frames(response: &Value, response_id: Option<&str>) -> Vec<Vec<u8>> {
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut frames = Vec::new();
    for (output_index, item) in output.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(kind) = object.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(kind, "function_call" | "custom_tool_call") {
            continue;
        }
        let mut added = Map::new();
        added.insert(
            "type".to_owned(),
            Value::String("response.output_item.added".to_owned()),
        );
        if let Some(response_id) = response_id {
            added.insert(
                "response_id".to_owned(),
                Value::String(response_id.to_owned()),
            );
        }
        added.insert(
            "output_index".to_owned(),
            Value::Number(serde_json::Number::from(output_index)),
        );
        added.insert("item".to_owned(), item.clone());
        frames.push(encode_sse_frame(
            "response.output_item.added",
            &Value::Object(added),
        ));

        let event_type = if kind == "custom_tool_call" {
            "response.custom_tool_call_input.done"
        } else {
            "response.function_call_arguments.done"
        };
        let mut arguments = Map::new();
        arguments.insert("type".to_owned(), Value::String(event_type.to_owned()));
        if let Some(response_id) = response_id {
            arguments.insert(
                "response_id".to_owned(),
                Value::String(response_id.to_owned()),
            );
        }
        arguments.insert(
            "output_index".to_owned(),
            Value::Number(serde_json::Number::from(output_index)),
        );
        if let Some(id) = object.get("id") {
            arguments.insert("item_id".to_owned(), id.clone());
        }
        if let Some(call_id) = object.get("call_id") {
            arguments.insert("call_id".to_owned(), call_id.clone());
        }
        if kind == "custom_tool_call" {
            arguments.insert(
                "input".to_owned(),
                object
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            );
        } else {
            arguments.insert(
                "arguments".to_owned(),
                object
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            );
        }
        frames.push(encode_sse_frame(event_type, &Value::Object(arguments)));

        let mut done = Map::new();
        done.insert(
            "type".to_owned(),
            Value::String("response.output_item.done".to_owned()),
        );
        if let Some(response_id) = response_id {
            done.insert(
                "response_id".to_owned(),
                Value::String(response_id.to_owned()),
            );
        }
        done.insert(
            "output_index".to_owned(),
            Value::Number(serde_json::Number::from(output_index)),
        );
        done.insert("item".to_owned(), item.clone());
        frames.push(encode_sse_frame(
            "response.output_item.done",
            &Value::Object(done),
        ));
    }
    frames
}

fn encode_sse_frame(event_type: &str, value: &Value) -> Vec<u8> {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    format!("event: {event_type}\ndata: {data}\n\n").into_bytes()
}

async fn send_relay_failure(
    sender: &gateway_plugin_sdk::client::MiddlewareBodySender,
    response_id: Option<&str>,
) -> Result<(), gateway_plugin_sdk::client::SessionError> {
    let response = json!({
        "id": response_id.unwrap_or("resp_relay_error"),
        "status": "failed",
        "output": [],
        "error": {
            "type": "invalid_request_error",
            "code": "relay_error",
            "message": "Basis Points returned an invalid client tool relay."
        }
    });
    let event = json!({"type": "response.failed", "response": response});
    sender
        .send(MiddlewareBodyFrame::new(
            encode_sse_frame("response.failed", &event),
            true,
        ))
        .await
}

fn transform_json_response(relay: &RelayContext, body: &[u8]) -> Result<Vec<u8>, RelayError> {
    if let Ok(response) = serde_json::from_slice::<Value>(body) {
        return transform_json_value(relay, &response, body);
    }
    for event in sse::parse_sse_events(body)
        .into_iter()
        .filter(|event| !event.is_done())
    {
        let event = sse::parse_json_event(&event).map_err(|_| RelayError {
            status: 502,
            code: "invalid_upstream_response",
            message: "Basis Points returned invalid JSON/SSE".to_owned(),
        })?;
        if matches!(
            event.event_type(),
            Some("response.completed" | "response.incomplete")
        ) {
            let response = event.value().get("response").unwrap_or(event.value());
            return serde_json::to_vec(&relay.transform_response(response)?.response).map_err(
                |_| RelayError {
                    status: 502,
                    code: "relay_encoding_failed",
                    message: "client tool relay response could not be encoded".to_owned(),
                },
            );
        }
    }
    Err(RelayError {
        status: 502,
        code: "invalid_upstream_response",
        message: "Basis Points returned no terminal JSON response".to_owned(),
    })
}

fn transform_json_value(
    relay: &RelayContext,
    response: &Value,
    original: &[u8],
) -> Result<Vec<u8>, RelayError> {
    let transformed = relay.transform_response(response)?;
    if transformed.changed {
        serde_json::to_vec(&transformed.response).map_err(|_| RelayError {
            status: 502,
            code: "relay_encoding_failed",
            message: "client tool relay response could not be encoded".to_owned(),
        })
    } else {
        Ok(original.to_vec())
    }
}

fn relay_error_fault(code: &str, message: &str) -> PluginFault {
    PluginFault::new(
        gateway_plugin_sdk::ErrorCode::Fault,
        format!("client tool relay failed ({code}): {message}"),
    )
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
        if character.is_ascii_alphanumeric()
            || character == '.'
            || character == '-'
            || character == '_'
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::ToolRelay;

    #[test]
    fn synthetic_sse_frames_use_real_event_boundaries() {
        assert_eq!(
            encode_sse_frame("response.completed", &json!({"type": "response.completed"})),
            b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n"
        );
    }

    #[test]
    fn no_relay_sse_delivery_preserves_frames_and_drops_done() {
        let mut delivery = RelaySseDelivery::new(None);
        assert_eq!(
            delivery.accept(b"event: text\ndata: hello\n\n".to_vec(), false),
            Ok(vec![b"event: text\ndata: hello\n\n".to_vec()])
        );
        assert_eq!(
            delivery.accept(b"data: [DONE]\n\n".to_vec(), true),
            Ok(Vec::new())
        );
        assert_eq!(delivery.finish(), Ok(Vec::new()));
    }

    #[test]
    fn json_relay_preserves_non_tool_response_bytes() {
        let source = json!({
            "tools": [{"type": "function", "name": "exec", "parameters": {"type": "object"}}]
        })
        .as_object()
        .expect("object source")
        .clone();
        let prepared = ToolRelay::new()
            .prepare_source(&source)
            .expect("relay preparation");
        let body = br#"{"id":"resp_1","status":"completed","output":[]}"#;
        assert_eq!(
            transform_json_response(prepared.relay_context(), body).expect("response transform"),
            body
        );
    }
}
