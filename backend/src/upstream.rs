use gateway_plugin_sdk::{
    PluginFault,
    call::host::{HttpResponse, StreamClose, StreamRead},
    client::{HostClient, HostReply, SessionError},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub const USER_AGENT: &str = concat!("cpr-oai-basispoints/", env!("CARGO_PKG_VERSION"));

/// 与 CPA 插件相同的 Basis Points 客户端画像；access_token 不进入任何日志。
#[must_use]
pub fn auth_headers(access_token: &str, account_id: &str, stream: bool) -> Vec<(String, String)> {
    let accept = if stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    vec![
        ("authorization".to_owned(), format!("Bearer {access_token}")),
        ("chatgpt-account-id".to_owned(), account_id.to_owned()),
        ("x-openai-account-id".to_owned(), account_id.to_owned()),
        ("x-basispoints-auth-mode".to_owned(), "chatgpt".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), accept.to_owned()),
        ("accept-encoding".to_owned(), "identity".to_owned()),
        ("origin".to_owned(), "https://bps.openai.com".to_owned()),
        (
            "x-openai-internal-basispoints-client-agent-profile".to_owned(),
            "excel".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-editor".to_owned(),
            "excel".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-host".to_owned(),
            "office".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-platform".to_owned(),
            "excel".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-platform-class".to_owned(),
            "PC".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-product".to_owned(),
            "basispoints-excel-plugin".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-client-runtime".to_owned(),
            "desktop".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-office-host".to_owned(),
            "Excel".to_owned(),
        ),
        (
            "x-openai-internal-basispoints-office-platform".to_owned(),
            "PC".to_owned(),
        ),
        ("x-stainless-arch".to_owned(), "unknown".to_owned()),
        ("x-stainless-lang".to_owned(), "js".to_owned()),
        ("x-stainless-os".to_owned(), "Unknown".to_owned()),
        (
            "x-stainless-package-version".to_owned(),
            "6.31.0".to_owned(),
        ),
        ("x-stainless-retry-count".to_owned(), "0".to_owned()),
        (
            "x-stainless-runtime".to_owned(),
            "browser:chrome".to_owned(),
        ),
        ("user-agent".to_owned(), USER_AGENT.to_owned()),
    ]
}

/// 发起受管 HTTP 流式请求，返回响应头与流句柄；调用方负责读取与关闭。
pub async fn open_stream(
    host: &HostClient,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<HttpResponse, PluginFault> {
    let reply = host
        .call(
            "host.http.do_stream",
            json!({
                "method": "POST",
                "url": url,
                "headers": headers,
            }),
            body,
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let response: HttpResponse = decode(reply.result)?;
    if response.stream.is_none() {
        return Err(fault("宿主未返回上游流句柄"));
    }
    Ok(response)
}

/// 读取流式请求的错误正文（非 2xx 时上游仍会给出响应流）。
pub async fn read_full_stream(
    host: &HostClient,
    stream_id: &str,
    maximum_bytes: usize,
) -> Result<Vec<u8>, PluginFault> {
    let mut buffer = Vec::new();
    loop {
        let (eof, chunk) = read_stream_chunk(host, stream_id).await?;
        if buffer.len() + chunk.len() > maximum_bytes {
            let _ = close_stream(host, stream_id).await;
            return Err(fault("上游错误正文超过响应大小上限"));
        }
        buffer.extend_from_slice(&chunk);
        if eof {
            return Ok(buffer);
        }
    }
}

pub async fn read_stream_chunk(
    host: &HostClient,
    stream_id: &str,
) -> Result<(bool, Vec<u8>), PluginFault> {
    let reply = host
        .call(
            "host.http.stream_read",
            json!(StreamRead {
                stream: stream_id.to_owned(),
                maximum_bytes: 64 * 1024,
            }),
            Vec::new(),
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let eof = reply
        .result
        .get("eof")
        .and_then(Value::as_bool)
        .ok_or_else(|| fault("宿主流读取响应无效"))?;
    Ok((eof, reply.payload))
}

pub async fn close_stream(host: &HostClient, stream_id: &str) -> Result<(), PluginFault> {
    let reply = host
        .call(
            "host.http.stream_close",
            json!(StreamClose {
                stream: stream_id.to_owned(),
            }),
            Vec::new(),
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let _ = decode::<Value>(reply.result)?;
    Ok(())
}

/// 非流式受管 HTTP 请求；响应正文整体随 reply payload 返回。
pub async fn do_request(
    host: &HostClient,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<(HttpResponse, Vec<u8>), PluginFault> {
    let reply = host
        .call(
            "host.http.do",
            json!({
                "method": "POST",
                "url": url,
                "headers": headers,
            }),
            body,
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let response: HttpResponse = decode(reply.result)?;
    Ok((response, reply.payload))
}

/// 账号域回调：输入输出都是二进制 JSON，控制参数固定为 `{}`。
pub async fn auth_call<I, O>(host: &HostClient, method: &str, input: &I) -> Result<O, PluginFault>
where
    I: serde::Serialize,
    O: DeserializeOwned,
{
    let HostReply { result, payload } = host
        .call(
            method,
            json!({}),
            serde_json::to_vec(input).map_err(|_| fault("请求编码失败"))?,
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    if !result.is_null() && result != json!({}) {
        return Err(fault("宿主账号回调信封无效"));
    }
    serde_json::from_slice(&payload).map_err(|_| fault("宿主账号回调结果无效"))
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, PluginFault> {
    serde_json::from_value(value).map_err(|_| fault("宿主回调结果无效"))
}

pub(crate) fn fault(message: &'static str) -> PluginFault {
    PluginFault::new(gateway_plugin_sdk::ErrorCode::Fault, message)
}

/// 泵任务里的轻量留痕；仅记录事件名，不携带任何业务数据。
pub async fn trace_fault(host: &HostClient, step: &'static str) {
    let request = gateway_plugin_sdk::call::host::LogRequest {
        event: format!("oai-basispoints.{step}"),
        level: gateway_plugin_sdk::call::host::LogLevel::Info,
        fields: Default::default(),
    };
    let _ = host
        .call(
            "host.log",
            serde_json::to_value(&request).unwrap_or(json!({})),
            Vec::new(),
        )
        .await;
}
