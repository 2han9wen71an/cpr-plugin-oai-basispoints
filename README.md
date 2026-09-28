# cpr-plugin-oai-basispoints

Codex Proxy RS（CPR）网关插件：复用宿主已有的 Codex OAuth 凭据接入
[OpenAI Basis Points](https://bps.openai.com)（`/basispoints/api/responses`），
对外提供 `*-basispoints` 别名模型的 Responses 代理。

移植自 CLIProxyAPI（CPA）插件
[JaxsonWang/cpa-plugin-oai-basispoints](https://github.com/JaxsonWang/cpa-plugin-oai-basispoints)（MIT），
按 CPR 插件系统的合同重写为 Rust 子进程插件。

## 工作原理

CPA 插件依赖 `auth.parse`／`executor.execute` 等 Provider 扩展点；CPR 的 Provider 固定为内置
OpenAI 与 xAI，插件不能注册执行器，因此本插件采用**短路中间件**架构：

```text
客户端请求 model=gpt-6-astra-basispoints
        │
        ▼
[model_catalog] 发布别名（形式指向原生 openai 模型，仅作目录载体）
        │
        ▼
[middleware.request] 拦截别名模型 ── 其余模型原样放行（next）
        │ 不调用 next（短路，Core 不结算）
        ▼
[host.auth.list/get] 读取 openai OAuth 账号凭据（accounts 域，插件内轮换）
        │
        ▼
[host.http.do / do_stream] 受管出站 HTTP → bps.openai.com
        │
        ▼
上游 SSE 帧（raw frame）→ 短路响应流回客户端
```

短路响应不进宿主账本与调度（Core 对不调 `next` 的正文不结算），账号由插件在
启用的 `openai` OAuth 账号间轮换选择；并发、熔断、亲和、额度观测对这条路径不生效。

## v0.1 能力边界

- ✅ 纯聊天（无工具）的流式（SSE）与非流式 Responses 请求
- ✅ `instructions` → developer 序言；`reasoning.effort` 归一化
  （`low/medium/high/xhigh/ultra`，`max/x-high/extra-high` → `xhigh`，其余回落 `medium`）
- ✅ `prompt_cache_key`/`session_id` 透传为 `prompt_cache_key`；`task_id`/`turn_id`
  按 CPA 同构的 uuid-v5 规则稳定生成
- ✅ reasoning 密文条目保留、`item_reference`/`additional_tools` 丢弃
- ❌ `previous_response_id`、非标准 `service_tier`、结构化 `text.format` → 明确 400
- ❌ 客户端工具中继协议（`tools`/`tool_choice` 携带者 → 明确 400；Codex CLI 等
  必带工具的客户端暂不可用）
- ❌ 附件/图片上传、WS 上游传输、源凭据管理页

## 安装

1. `cargo build --release`（在目标平台或容器内构建）
2. `cpr-plugin package --manifest plugin.json --binary backend/target/<triple>/release/cpr-plugin-oai-basispoints --target <triple> --output-dir dist`
3. 在 CPR 管理面上传 `dist/*.tar.gz`，按 `configurationSchema` 配置：

| 配置项 | 说明 | 默认 |
| --- | --- | --- |
| `responsesUrl` | Basis Points Responses 接口地址 | `https://bps.openai.com/basispoints/api/responses` |
| `models` | `[{alias, upstreamModel}]` 别名与上游模型映射 | `gpt-6-astra-basispoints → gpt-6-astra` |
| `catalogTarget` | 别名形式指向的原生 openai 模型（须存在于当前目录） | `gpt-5.3-codex` |
| `maxResponseBytes` | 上游响应体上限 | 64 MiB |

## 权限

`network`（受管出站 HTTP）、`accounts`（读取 OAuth 凭据）、`requests`（请求正文投影）。
凭据只在内存中使用，不落日志、不写回账号文件。

## License

MIT（协议适配层源自 JaxsonWang/cpa-plugin-oai-basispoints，遵循其 MIT 授权）
