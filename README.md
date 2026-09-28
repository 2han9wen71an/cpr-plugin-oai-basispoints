# cpr-plugin-oai-basispoints

Codex Proxy RS（CPR）网关插件：复用宿主已有的 Codex OAuth 凭据接入 [OpenAI Basis Points](https://bps.openai.com)（`/basispoints/api/responses`），对外提供 `*-basispoints` 别名模型的 Responses 代理。

移植自 CLIProxyAPI（CPA）插件 [JaxsonWang/cpa-plugin-oai-basispoints](https://github.com/JaxsonWang/cpa-plugin-oai-basispoints)（MIT），按 CPR 官方插件系统（3.16.0+）规范重写为高性能 Rust 子进程插件。

---

## 工作原理

CPA 插件依赖 `auth.parse` 与 `executor.execute` 等 Provider 扩展点；CPR 的 Provider 固定为内置 OpenAI 与 xAI，插件无法注册底层执行器。因此本插件采用 **短路中间件（Short-circuit Middleware）** 架构实现：

```text
客户端请求 model=gpt-6-astra-basispoints
        │
        ▼
[model_catalog] 发布别名（指向原生 openai 模型作为目录载体）
        │
        ▼
[middleware.request] 拦截别名模型 ── 其余模型原样放行（next）
        │ 不调用 next（短路响应，Core 不计费不进账本）
        ▼
[host.auth.list/get] 读取 openai OAuth 账号凭据（accounts 域，插件内自动挑选 ready 账号轮询）
        │
        ▼
[host.http.do_stream] 受管出站 HTTP/2 → bps.openai.com
        │
        ▼
上游 SSE 帧流式代理 → 逐帧过滤转发回客户端
```

短路响应不调用 `next`，根据 CPR 引擎规范，无需进入 Core 账本计费。账号由插件在启用的 `openai` OAuth 账号间自动选择；并发、熔断、亲和、额度观测等原生调度策略对短路路径不生效。

---

## 能力边界

- ✅ **流式代理**：支持完整 SSE（`stream: true`）实时吐字，首包返回 Excel 专用 Agent 提示词，逐帧交付 `response.output_text.delta`。
- ✅ **非流式请求**：支持非流式 JSON 返回。
- ✅ **协议规范化**：
  - `instructions` 自动转为 developer 序言前置。
  - `reasoning.effort` 归一化（`low/medium/high/xhigh/ultra`，`max/x-high/extra-high` 映射为 `xhigh`，缺省平滑回落 `medium`）。
  - `prompt_cache_key`/`session_id` 自动透传。
  - `task_id`/`turn_id` 依 CPA 同构的 UUIDv5 算法生成，支持上下文连贯。
- ✅ **上游错误穿透**：忠实穿透上游 401（未授权/无权限）与 429（用量限流）状态码及脱敏正文，不产生 502。
- ❌ **前置拦截校验**：携带 `previous_response_id`、非标准 `service_tier`、结构化 `text.format` 时明确返回 HTTP 400。
- ❌ **暂不支持工具中继**：携带客户端工具（`tools`/`tool_choice`）请求明确返回 400（Codex CLI 等强制工具调用的客户端暂不可用）。

---

## 在线安装与更新指南

插件已发布自动化构建版本，提供 `linux-x86_64` (AMD64) 与 `linux-aarch64` (ARM64) 纯静态（musl）构建，适配所有主流 Linux 宿主环境。

### 方式一：通过 CPR 在线更新源安装（推荐，一键自动更新）

1. 进入 CPR 管理后台（Web UI）。
2. 在左侧菜单进入 **插件中心** $\rightarrow$ **更新来源**。
3. 点击 **添加更新来源**：
   - **插件标识**：`2han9wen71an.oai-basispoints`
   - **来源类型**：`GitHub`
   - **仓库**：`2han9wen71an/cpr-plugin-oai-basispoints`
   - **检查策略**：`稳定版 (Stable)`
4. 保存后点击 **检查更新**，CPR 会自动读取 GitHub Releases 并比对 `checksums.txt` 校验和。
5. 选择匹配当前宿主架构（`linux-x86_64` 或 `linux-aarch64`）的产物，点击 **安装**。

### 方式二：手动下载 Release 归档包安装

1. 前往本仓库 [GitHub Releases 页面](https://github.com/2han9wen71an/cpr-plugin-oai-basispoints/releases)。
2. 根据服务器架构下载最新的归档包：
   - Linux ARM64（如 Oracle Cloud ARM、树莓派等）：`2han9wen71an.oai-basispoints-<version>-linux-aarch64.tar.gz`
   - Linux AMD64（Intel / AMD 服务器）：`2han9wen71an.oai-basispoints-<version>-linux-x86_64.tar.gz`
3. 进入 CPR 管理后台，进入 **插件中心** $\rightarrow$ **插件制品**。
4. 点击 **上传插件包**，选择下载的 `.tar.gz` 文件上传。
5. 审核确认请求的权限（联网、账号与凭据、请求处理），点击 **接受并安装**。

---

## 插件配置与启用

安装制品后，在 **插件实例** 页面点击该插件进行配置与发布：

### 1. 配置参数 (`configuration`)

| 配置项 | 类型 | 必填 | 默认值 | 说明 |
| --- | --- | :---: | --- | --- |
| `responsesUrl` | 字符串 | 是 | `https://bps.openai.com/basispoints/api/responses` | Basis Points 接口地址 |
| `models` | 数组 | 是 | `[{"alias":"gpt-6-astra-basispoints","upstreamModel":"gpt-6-astra"}]` | 公开别名与上游模型映射 |
| `catalogTarget` | 字符串 | 否 | `gpt-5.3-codex` | 目录别名形式绑定的原生 openai 模型（请求不会真正路由到它） |
| `maxResponseBytes` | 整数 | 否 | `67108864` (64 MiB) | 上游单次响应大小上限 |

**配置示例**：
```json
{
  "responsesUrl": "https://bps.openai.com/basispoints/api/responses",
  "models": [
    {
      "alias": "gpt-6-astra-basispoints",
      "upstreamModel": "gpt-6-astra"
    },
    {
      "alias": "gpt-5.6-sol-basispoints",
      "upstreamModel": "gpt-5.6-sol"
    }
  ],
  "catalogTarget": "gpt-6-astra",
  "maxResponseBytes": 67108864
}
```

### 2. 数据面绑定 (`bindings`)

在实例配置页面的 **功能绑定** 中确认或添加中间件绑定：
- **能力**：`middleware` (`2han9wen71an.oai-basispoints.middleware`)
- **阶段**：`request`
- **模型**：选择或填入配置中的别名（如 `gpt-6-astra-basispoints`、`gpt-5.6-sol-basispoints`）
- **失败策略**：`reject`（拦截失败时返回错误，避免误穿透到未配置的原生上游）

保存后点击 **发布** 即可生效。

---

## 客户端调用示例

通过标准 OpenAI Responses 接口调用已发布的别名模型：

```bash
curl -N http://<cpr-host>:<port>/v1/responses \
  -H "Authorization: Bearer <your-cpr-client-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-6-astra-basispoints",
    "input": [
      {
        "role": "user",
        "content": "Hello! Please summarize how Basis Points works."
      }
    ],
    "stream": true
  }'
```

---

## 本地构建

如需自行修改并构建：

```bash
# 1. 运行测试
cd backend && cargo test

# 2. 编译 Linux 目标产物（推荐使用 cargo-zigbuild 或在 docker 中构建）
cargo zigbuild --release --target aarch64-unknown-linux-musl --manifest-path backend/Cargo.toml
cargo zigbuild --release --target x86_64-unknown-linux-musl --manifest-path backend/Cargo.toml

# 3. 打包生成符合 CPR 规范的归档包
python3 scripts/package.py --binary backend/target/aarch64-unknown-linux-musl/release/cpr-plugin-oai-basispoints --arch aarch64
python3 scripts/package.py --binary backend/target/x86_64-unknown-linux-musl/release/cpr-plugin-oai-basispoints --arch x86_64
```

---

## 权限声明

本插件请求以下权限：
- `network`：向 `bps.openai.com` 发起受管出站流式 HTTP 请求。
- `accounts`：枚举并获取启用的 `openai` OAuth 账号 Access Token 与 ChatGPT-Account-ID（仅保留在内存中临时用于请求签名，绝不记录日志或写回存储）。
- `requests`：读取待代理的请求正文以解析模型、对话历史及序言参数。

---

## License

MIT License（协议转换逻辑源自 [JaxsonWang/cpa-plugin-oai-basispoints](https://github.com/JaxsonWang/cpa-plugin-oai-basispoints)，遵循其开源协议）。
