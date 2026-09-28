use std::{sync::Arc, time::Duration};

use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Message, PROTOCOL_VERSION, Stage,
    call::middleware::{MiddlewareRequestHead, MiddlewareResponseHead},
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use tokio::{
    io::{DuplexStream, ReadHalf, WriteHalf},
    task::JoinHandle,
};

use cpr_plugin_oai_basispoints as plugin_crate;

const MAXIMUM_STREAM_CHUNK_BYTES: usize = 64 * 1024;

struct HostPeer {
    reader: ReadHalf<DuplexStream>,
    writer: WriteHalf<DuplexStream>,
}

async fn start_session(
    handler: gateway_plugin_sdk::client::ComposedPlugin,
    contributes: gateway_plugin_sdk::Contributions,
) -> (
    HostPeer,
    JoinHandle<Result<(), gateway_plugin_sdk::client::SessionError>>,
) {
    let (host, plugin_side) = tokio::io::duplex(MAXIMUM_STREAM_CHUNK_BYTES * 2);
    let (plugin_reader, plugin_writer) = tokio::io::split(plugin_side);
    let task = tokio::spawn(async move {
        gateway_plugin_sdk::client::PluginSession::accept(
            plugin_reader,
            plugin_writer,
            gateway_plugin_sdk::client::SessionConfig {
                maximum_stream_chunk_bytes: MAXIMUM_STREAM_CHUNK_BYTES,
                maximum_calls: 8,
                maximum_callbacks: 8,
                maximum_buffered_stream_chunks: 64,
                handshake_timeout: Duration::from_secs(2),
                maximum_call_timeout: Duration::from_secs(5),
            },
        )
        .await?
        .run(handler)
        .await
    });
    let (mut reader, mut writer) = tokio::io::split(host);
    write_frame(
        &mut writer,
        &Frame::control(Message::Hello {
            handshake: Handshake {
                protocol_version: PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: plugin_crate::PLUGIN_ID.into(),
                instance_id: "test-instance".into(),
                generation: 1,
                incarnation: "test-incarnation".into(),
                configuration: json!({
                    "responsesUrl": "https://bps.openai.com/basispoints/api/responses",
                    "models": [{"alias": "gpt-6-astra-basispoints", "upstreamModel": "gpt-6-astra"}],
                    "catalogTarget": "gpt-5.3-codex",
                    "maxResponseBytes": 67108864
                }),
                permissions: vec![
                    gateway_plugin_sdk::Permission::Network,
                    gateway_plugin_sdk::Permission::Accounts,
                    gateway_plugin_sdk::Permission::Requests,
                ],
                contributes,
            },
        }),
    )
    .await
    .unwrap();
    let ready = read_frame(&mut reader).await.unwrap();
    assert!(matches!(
        ready,
        Frame {
            message: Message::Ready { .. },
            ref payload,
        } if payload.is_empty()
    ));
    (HostPeer { reader, writer }, task)
}

async fn send_call(
    host: &mut HostPeer,
    id: u64,
    method: &str,
    stage: Stage,
    params: Value,
    payload: Vec<u8>,
) {
    write_frame(
        &mut host.writer,
        &Frame {
            message: Message::Call {
                id,
                method: method.into(),
                context: CallContext {
                    call_id: id,
                    instance_id: "test-instance".into(),
                    generation: 1,
                    incarnation: "test-incarnation".into(),
                    stage,
                    timeout_ms: 2_000,
                    resource_scope_id: format!("scope-{id}"),
                    request_id: Some("req-test".into()),
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params,
            },
            payload,
        },
    )
    .await
    .unwrap();
}

async fn receive(host: &mut HostPeer) -> Frame {
    tokio::time::timeout(Duration::from_secs(3), read_frame(&mut host.reader))
        .await
        .expect("plugin response timed out")
        .expect("plugin response frame must be valid")
}

fn request_head(model: &str) -> Value {
    let head = MiddlewareRequestHead {
        request_id: "req-test".into(),
        mount: gateway_plugin_sdk::call::middleware::MiddlewareMount::Request,
        attempt_index: None,
        operation: "responses.create".into(),
        protocol: "openai".into(),
        endpoint: "/v1/responses".into(),
        transport: gateway_plugin_sdk::call::middleware::MiddlewareTransport::HttpJson,
        provider: Some("openai".into()),
        model: Some(model.into()),
        account_id: None,
        headers: vec![gateway_plugin_sdk::call::middleware::MiddlewareHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body_visible: true,
    };
    serde_json::to_value(&head).unwrap()
}

async fn run_guard_case(body: Value) -> Result<MiddlewareResponseHead, String> {
    let manifest = plugin_crate::manifest().unwrap();
    let contributes = manifest.contributes.clone();
    let config = Arc::new(
        plugin_crate::RuntimeConfig::from_configuration(&json!({
            "responsesUrl": "https://bps.openai.com/basispoints/api/responses",
            "models": [{"alias": "gpt-6-astra-basispoints", "upstreamModel": "gpt-6-astra"}]
        }))
        .unwrap(),
    );
    let handler = plugin_crate::plugin(config).unwrap();
    let (mut host, _task) = start_session(handler, contributes).await;
    send_call(
        &mut host,
        1,
        "middleware.handle",
        Stage::Request,
        request_head("gpt-6-astra-basispoints"),
        serde_json::to_vec(&body).unwrap(),
    )
    .await;
    // 运行时语义：Call 后立即授予初始 credit，然后处理回调/流帧。
    grant_credit(&mut host, 1).await;
    let mut outcome = Err("no reply".to_owned());
    loop {
        let frame = receive(&mut host).await;
        match frame.message {
            Message::Callback { id, .. } => {
                reply_callback(&mut host, id).await;
            }
            Message::Result { id: 1, result } => {
                eprintln!(
                    "[probe] Result JSON: {}",
                    serde_json::to_string(&result).unwrap()
                );
                outcome = Ok(serde_json::from_value(result).unwrap());
            }
            Message::Error { id: 1, error } => {
                outcome = Err(serde_json::to_string(&error).unwrap());
            }
            Message::Stream { id: 1, .. } => {
                eprintln!(
                    "[probe] Stream payload len={}, bytes={:?}",
                    frame.payload.len(),
                    &frame.payload[..frame.payload.len().min(32)]
                );
            }
            Message::End { id: 1, .. } => break,
            unexpected => panic!("unexpected frame: {unexpected:?}"),
        }
    }
    outcome
}

async fn grant_credit(host: &mut HostPeer, id: u64) {
    write_frame(
        &mut host.writer,
        &Frame {
            message: Message::Credit {
                id,
                bytes: 256 * 1024,
                frames: 32,
            },
            payload: Vec::new(),
        },
    )
    .await
    .unwrap();
}

async fn reply_callback(host: &mut HostPeer, id: u64) {
    write_frame(
        &mut host.writer,
        &Frame {
            message: Message::Result {
                id,
                result: json!({"recorded": true}),
            },
            payload: Vec::new(),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn guard_rejection_returns_direct_400_without_host_callbacks() {
    let outcome = run_guard_case(json!({
        "model": "gpt-6-astra-basispoints",
        "stream": false,
        "input": "hi",
        "previous_response_id": "resp_x"
    }))
    .await;
    match outcome {
        Ok(head) => {
            assert_eq!(head.response, None, "guard path must short-circuit");
            assert_eq!(head.status, Some(400));
            assert_eq!(head.protocol.as_deref(), Some("openai"));
        }
        Err(error) => panic!("plugin returned fault instead of 400 response: {error}"),
    }
}

#[tokio::test]
async fn unknown_model_passes_through_via_next() {
    // 非别名模型：插件会调用 next；假宿主没有实现 next 回调，
    // 因此预期收到一个对 host.middleware.next 的 Call —— 这里验证的是
    // 插件确实发起了透传而不是直接短路。
    let manifest = plugin_crate::manifest().unwrap();
    let contributes = manifest.contributes.clone();
    let config = Arc::new(
        plugin_crate::RuntimeConfig::from_configuration(&json!({
            "responsesUrl": "https://bps.openai.com/basispoints/api/responses",
            "models": [{"alias": "gpt-6-astra-basispoints", "upstreamModel": "gpt-6-astra"}]
        }))
        .unwrap(),
    );
    let handler = plugin_crate::plugin(config).unwrap();
    let (mut host, task) = start_session(handler, contributes).await;
    send_call(
        &mut host,
        1,
        "middleware.handle",
        Stage::Request,
        request_head("probe-model"),
        serde_json::to_vec(&json!({"model": "probe-model", "input": "hi"})).unwrap(),
    )
    .await;
    let mut saw_next = false;
    let session_result: Result<(), String>;
    loop {
        let frame = match tokio::time::timeout(Duration::from_secs(3), read_frame(&mut host.reader))
            .await
            .expect("receive timed out")
        {
            Ok(frame) => frame,
            Err(error) => {
                let session = task.await.map_err(|e| e.to_string());
                session_result = match session {
                    Ok(inner) => Err(format!("session: {inner:?}")),
                    Err(join) => Err(format!("join: {join}")),
                };
                panic!("connection closed by plugin: {error}; session_result={session_result:?}");
            }
        };
        match frame.message {
            Message::Callback { id, method, .. } => {
                if method == "host.middleware.next" {
                    saw_next = true;
                    break;
                }
                reply_callback(&mut host, id).await;
            }
            Message::Result { id: 1, .. } | Message::Error { id: 1, .. } => break,
            Message::Stream { id: 1, .. } | Message::End { id: 1, .. } => {}
            unexpected => panic!("unexpected frame: {unexpected:?}"),
        }
    }
    assert!(
        saw_next,
        "plugin must pass non-alias models through to next"
    );
    let _ = write_frame(&mut host.writer, &Frame::control(Message::Shutdown)).await;
    task.abort();
    let _ = task.await;
}
