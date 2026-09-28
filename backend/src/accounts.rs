use std::sync::atomic::{AtomicUsize, Ordering};

use gateway_plugin_sdk::call::host::{AuthCredential, AuthGetRequest, AuthListRequest};
use serde_json::Value;

use crate::upstream::auth_call;

/// 从宿主账号池轮换挑选一个可用的 openai 账号并读取其 Codex OAuth 凭据。
///
/// 注意：这是插件自身的选择，不经过宿主调度器；并发、熔断与额度观测
/// 对短路请求不生效。
#[derive(Default)]
pub struct AccountPicker {
    counter: AtomicUsize,
}

pub struct SelectedCredential {
    pub access_token: String,
    pub chatgpt_account_id: String,
}

impl AccountPicker {
    pub const fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
        }
    }

    /// # 错误
    ///
    /// 账号列表为空或凭据缺少 access_token 时返回描述性错误。
    pub async fn select(
        &self,
        host: &gateway_plugin_sdk::client::HostClient,
    ) -> Result<SelectedCredential, String> {
        let mut cursor = None;
        let mut candidates: Vec<(String, Option<i64>)> = Vec::new();
        loop {
            let page: gateway_plugin_sdk::call::host::AuthListResult = auth_call(
                host,
                "host.auth.list",
                &AuthListRequest {
                    provider_id: Some("openai".to_owned()),
                    cursor: cursor.clone(),
                    limit: 200,
                },
            )
            .await
            .map_err(|error| format!("枚举账号失败：{error:?}"))?;
            for account in &page.accounts {
                if account.enabled
                    && account.authentication_kind == "oauth"
                    && account.credential_state == "ready"
                {
                    candidates.push((
                        account.account_id.clone(),
                        account.access_token_expires_at_ms,
                    ));
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        if candidates.is_empty() {
            return Err("宿主没有启用的 openai OAuth 账号".to_owned());
        }
        // 优先非过期账号；全部过期时仍取过期最晚的一个，让上游给出明确错误。
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        let mut usable: Vec<&(String, Option<i64>)> = candidates
            .iter()
            .filter(|(_, expiry)| expiry.is_none_or(|at| at > now_ms))
            .collect();
        if usable.is_empty() {
            usable = candidates
                .iter()
                .max_by_key(|(_, expiry)| *expiry)
                .into_iter()
                .collect();
        }
        let index = if usable.len() == 1 {
            0
        } else {
            self.counter.fetch_add(1, Ordering::Relaxed) % usable.len()
        };
        let account_id = usable[index].0.clone();
        let credential: AuthCredential = auth_call(
            host,
            "host.auth.get",
            &AuthGetRequest {
                account_id: account_id.clone(),
            },
        )
        .await
        .map_err(|error| format!("读取凭据失败：{error:?}"))?;
        let material = &credential.facts.material;
        let Some(access_token) = find_access_token(material) else {
            return Err(format!("账号 {account_id} 的凭据缺少 access_token"));
        };
        let chatgpt_account_id = find_account_id(material, &credential, &access_token)
            .ok_or_else(|| format!("账号 {account_id} 缺少 chatgpt account id"))?;
        Ok(SelectedCredential {
            access_token,
            chatgpt_account_id,
        })
    }
}

fn find_access_token(material: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(token) = material.get("access_token").and_then(Value::as_str) {
        let token = token.trim().strip_prefix("Bearer ").unwrap_or(token.trim());
        if !token.is_empty() {
            return Some(token.to_owned());
        }
    }
    for key in [
        "tokens",
        "token_data",
        "tokenData",
        "oauth",
        "sessionInfo",
        "session_info",
    ] {
        if let Some(nested) = material.get(key).and_then(Value::as_object)
            && let Some(token) = find_access_token(nested)
        {
            return Some(token);
        }
    }
    None
}

fn find_account_id(
    material: &serde_json::Map<String, Value>,
    credential: &AuthCredential,
    access_token: &str,
) -> Option<String> {
    if let Some(id) = credential.facts.upstream_account_id.as_deref()
        && !id.is_empty()
    {
        return Some(id.to_owned());
    }
    for key in ["chatgpt_account_id", "account_id", "accountId"] {
        if let Some(id) = material.get(key).and_then(Value::as_str)
            && !id.trim().is_empty()
        {
            return Some(id.trim().to_owned());
        }
    }
    jwt_claim(
        access_token,
        "https://api.openai.com/auth",
        "chatgpt_account_id",
    )
    .or_else(|| jwt_claim(access_token, "", "chatgpt_account_id"))
}

fn jwt_claim(token: &str, outer_key: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = decode_url_safe_base64(payload)?;
    let claims: Value = serde_json::from_str(&decoded).ok()?;
    if outer_key.is_empty() {
        return claims.get(claim).and_then(Value::as_str).map(str::to_owned);
    }
    claims
        .get(outer_key)
        .and_then(|auth| auth.get(claim))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn decode_url_safe_base64(input: &str) -> Option<String> {
    let mut buffer = input.trim().trim_end_matches('=').as_bytes().to_vec();
    let mut output = Vec::with_capacity(buffer.len() * 3 / 4 + 3);
    let mut quad = [0u8; 4];
    let mut quad_len = 0;
    for byte in buffer.drain(..) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => continue,
        };
        quad[quad_len] = value;
        quad_len += 1;
        if quad_len == 4 {
            output.push((quad[0] << 2) | (quad[1] >> 4));
            output.push((quad[1] << 4) | (quad[2] >> 2));
            output.push((quad[2] << 6) | quad[3]);
            quad_len = 0;
        }
    }
    match quad_len {
        2 => output.push(quad[0] << 2),
        3 => {
            output.push((quad[0] << 2) | (quad[1] >> 4));
            output.push((quad[1] << 4) | (quad[2] >> 2));
        }
        _ => {}
    }
    String::from_utf8(output).ok()
}
