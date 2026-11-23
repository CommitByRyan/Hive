# H07 模型网关 — 设计文档（R07 侦察产出）

> 状态：侦察完成，设计草案
> 侦察对象：Magpie `http://127.0.0.1:3425/v1`（OpenAI 兼容）
> 日期：2026-11-23
> 约束：本文件是 H07 唯一交付物；未写入任何 Rust 代码，未运行 cargo，未修改 `crates/` 与 `WHITEBOARD.md`。

## 0. 结论摘要

Magpie 不是最小 OpenAI 兼容实现，而是带扩展元数据的网关：`/v1/models` 直接给出每模型的 `tool_call`、`context_length`、`max_output_tokens`、`reasoning_levels`、`pricing` 等能力字段；非流式响应 `usage` 恒定存在；流式只在**最后一个 chunk** 携带 `usage`（含 `quota` 配额对象）；错误体 `code` 可能为 `null`，只能用 HTTP 状态 + `type` 分类；`max_tokens` 截断时 tool call 的 `arguments` 会被截成非法 JSON。据此，客户端设计以「宽容解析 + 按状态/类型分类错误 + 末块收口 usage + 截断续写」为核心。

## 1. 实测事实表

| # | 探测项 | 实际观察 | 对实现的影响 |
|---|---|---|---|
| 1 | `GET /v1/models` | 200；响应体为扩展结构：`{"object":"list","data":[...],"first_id","has_more":false,"last_id"}`。每个模型含 `id / display_name / context_length / context_window / max_input_tokens / max_output_tokens / modalities / owned_by / pricing(可选) / reasoning(bool) / reasoning_levels / supported_parameters / supported_reasoning_levels / tool_call(bool) / native_endpoints / type / created / created_at` | 能力表不必硬编码，可直接解析 `/v1/models` 生成；但 `pricing`、`native_endpoints` 可能缺字段，解析用 `Option` + 不拒绝未知字段 |
| 2 | 非流式 `POST /v1/chat/completions` | 200；头含 `X-Magpie-Model`、`X-Magpie-Provider`、`X-Request-Id`。响应 `choices[0].finish_reason="stop"`，`message` 只有 `content/role`；`usage` 存在且含 `completion_tokens`、`completion_tokens_details.reasoning_tokens`、`prompt_tokens`、`prompt_tokens_details{cache_write_tokens,cached_tokens}`、`total_tokens` | 非流式 `usage` 视为权威计量来源；必须解析 `reasoning_tokens`；对额外字段保持宽容（不 `deny_unknown_fields`） |
| 3 | 不存在的模型名 | HTTP 400；错误体 `{"error":{"code":null,"message":"WorkBuddy: model [does-not-exist] service info not found","param":null,"type":"invalid_request_error"}}` | `code` 可为 `null`，不能 `unwrap`；分类以 HTTP 状态 + `type` 为准；400 类直接不重试并透出清晰 message |
| 4 | `stream=true` | SSE：先出现 `: heartbeat` 注释行（保活），随后多条 `data: {json}`，以 `data: [DONE]` 结尾。首个 chunk `delta.role="assistant"` 且 `content=""`；中间 chunk `finish_reason=null`、`usage=null`；**最后一个 chunk** 才带 `finish_reason="stop"` 与完整 `usage`，其内含 `quota{model,percent,reset_at,shared_models}` 与 `credit` | 解析器必须跳过注释行、逐行以 `data:` 前缀切帧；`usage`/`finish_reason` 只能从末块收口；预算在流中途只能用估算，权威值在末块；`quota.percent` 可作为软限流信号 |
| 5 | 带 `tools` 请求 | 200；`message` 同时含 `content` 与 `tool_calls`。tool call 形如 `{id:"call_...",type:"function",function:{name,arguments}}`；`arguments` 是**字符串化的 JSON**。本探测因 `max_tokens=32` 被截断，`finish_reason="length"` 且 `arguments` 为 `{"city"`（非法 JSON） | tool call 参数是字符串 JSON，必须累积后二次 `serde_json::from_str`；`finish_reason="length"` + 参数不完整时必须续写（追加 `tool` 消息让模型补完）或显式判为截断失败，绝不能把半截参数当完整 JSON 执行 |
| 6 | 速率限制响应头 | 所有响应的响应头均无 `X-RateLimit-*`、`Retry-After`；唯一的限额信号是流式末块 `usage.quota.percent / reset_at`（在响应体，不在头） | 不能依赖标准限流头；429 场景需以 HTTP 429 + 错误体识别，退避重试；配额软信号可选读 `quota.percent` 做预警 |

未验证项（如实标注，未编造）：

- **鉴权行为**：本机 `OPENAI_API_KEY` 已设置（长度 164），全部请求用 `Authorization: Bearer` 成功；**未验证**缺 key / 错 key 的 401 响应体与是否强制鉴权。
- **429 / 5xx 真实响应体**：本次探测未触发限流或服务端错误，错误体结构只能从 400 推断，429/5xx 的具体 `type` 取值未验证。
- **`reasoning_content` / 思维链字段**：探测提示极短，`reasoning_tokens=0` 且 `message` 未出现思维链字段；推理模型是否回传可见思维链内容未验证。
- **多模态**：模型列表声明 `modalities.input` 含 `image`，未实际发送图片请求验证。

## 2. Rust 客户端设计（草案，仅文档）

JSON 全部为 snake_case，Rust 结构体统一 `#[serde(rename_all = "snake_case")]`，且一律 `#[serde(default)]` + 不拒绝未知字段（网关会增补字段，如 `quota`、`credit`、`extra_fields`）。

### 2.1 请求与响应类型

```rust
// 请求（hive-llm）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,        // 实测 Magpie 接受 max_tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Message {
    pub role: Role,                       // system | user | assistant | tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,     // role=tool 时必填
}

// 非流式响应
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: String,                   // "chat.completion"
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,             // 非流式恒定存在；流式末块才有
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub index: usize,
    pub finish_reason: Option<FinishReason>, // 允许 null
    pub message: Message,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,                     // "function"
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,                // 字符串化 JSON，需二次解析
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
    #[serde(default)]
    pub quota: Option<QuotaInfo>,         // Magpie 扩展
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaInfo {
    pub model: String,
    pub percent: f32,                     // 0..=100
    pub reset_at: i64,                    // unix 秒
}
```

### 2.2 流式增量类型（delta 事件）

```rust
// SSE 帧：先按行解析，再落到事件
pub enum SseEvent {
    Comment(String),     // ": heartbeat" 等，直接忽略
    Data(String),        // data: {...}
    Done,                // data: [DONE]
}

// 单个 chunk（object = "chat.completion.chunk"）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<Usage>,             // 仅末块非 null
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub index: usize,
    pub finish_reason: Option<FinishReason>,
    pub delta: Delta,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,          // 增量文本，需要拼接
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,                     // 多工具调用时区分
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,        // 增量片段，按 index 累积拼接
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,                          // 旧式 function calling 兼容
    Unknown(String),                       // 未识别值降级，不丢数据
}

impl<'de> Deserialize<'de> for FinishReason {
    // 手写：null -> None；已知字面量映射；未知字符串 -> Unknown(String)
}
```

### 2.3 tool_call 增量拼接

流式多工具调用按 `index` 分桶累积，`id/name` 取首个非空，`arguments` 逐片段 `push_str`：

```rust
pub struct StreamAccumulator {
    pub content: String,
    pub tool_calls: BTreeMap<usize, PendingToolCall>,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
}

pub struct PendingToolCall {
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,                // 累积后的字符串化 JSON
}

impl StreamAccumulator {
    pub fn apply(&mut self, chunk: &CompletionChunk) {
        for c in &chunk.choices {
            if let Some(d) = &c.delta {
                if let Some(t) = &d.content { self.content.push_str(t); }
                if let Some(tcs) = &d.tool_calls {
                    for tc in tcs {
                        let e = self.tool_calls.entry(tc.index).or_default();
                        if let Some(id) = &tc.id { if e.id.is_none() { e.id = Some(id.clone()); } }
                        if let Some(f) = &tc.function {
                            if let Some(n) = &f.name { if e.name.is_none() { e.name = Some(n.clone()); } }
                            if let Some(a) = &f.arguments { e.arguments.push_str(a); }
                        }
                    }
                }
            }
            self.finish_reason = c.finish_reason.clone();
        }
        if let Some(u) = &chunk.usage { self.usage = Some(u.clone()); }
    }
    // finish_reason == Length 时，检查每个 PendingToolCall.arguments 能否 from_str 成功；
    // 失败 -> TruncatedToolArguments，由上层决定续写或失败。
}
```

### 2.4 finish_reason 处理

- `Stop`：正常结束。
- `ToolCalls`：进入工具执行循环，`arguments` 必须能解析为 JSON；解析失败按协议错误处理（不静默执行）。
- `Length`：被 `max_tokens` 截断。文本场景可接受但需在事件中标记 `truncated`；工具场景若参数不完整则必须续写（追加 assistant 的 `tool_calls` + 一条 `role=tool` 占位让模型继续）或判为可重试的截断失败。
- `ContentFilter` / `FunctionCall` / `Unknown`：作为结构化错误/告警向上冒泡，保留原始字符串。

## 3. 错误与重试

分类原则：**先看 HTTP 状态，再看错误体 `type`；`code` 只是辅助，可能为 `null`。**

| 类别 | 状态/信号 | 动作 |
|---|---|---|
| 4xx（除 408/429） | 400/401/403/404/422… | **不重试**；映射为终端错误。401 属鉴权失败，绝不重试，也不把 key 写入日志/事件 |
| 408 请求超时 | 408 | 可重试（请求可能未被处理），纳入退避 |
| 429 限流 | 429 | 指数退避 + 抖动，尊重 `Retry-After`（若出现）；实测无该头，退避用固定基数 |
| 5xx | 500/502/503/504 | 指数退避 + 抖动，有上限 |
| 网络/连接中断 | reqwest 传输错误、EOF、流中途断开 | 非流式可重试；流式已收到首个 token 后**不再自动重试**（避免重复计费与副作用），改为上报中断 |
| 超时 | 连接超时/读超时/总超时 | 视为可重试，计入同一重试预算 |

统一策略（仅文档，不落代码）：

- 最大重试 `max_retries = 3`（总尝试次数 4），只对 429/5xx/408/传输错误/超时重试。
- 退避 `base = 500ms`，`delay = base * 2^attempt + jitter(0..250ms)`，封顶 `10s`。
- 幂等边界：Chat Completions 本身**非幂等**；只有「未观察到任何 token、未产生任何 tool call、未向外部登记副作用」的请求才允许安全重试。流式中途、已拿到 `finish_reason` 或已触发工具执行的请求一律不自动重试，改走上层 Node 重试（`attempt_no` 与租约 token 分离，见 PROTOCOL）。
- 所有重试可取消：退避 `sleep` 用 `tokio::select!` 挂在 `CancellationToken` 上。
- 每次重试记录事件（`retry_attempt`、退避毫秒、失败原因分类），供审计；不输出 key。

```rust
pub enum GatewayError {
    Api { status: u16, body: ApiErrorBody },       // 4xx/5xx
    Transport(String),                              // reqwest 层错误
    Timeout,
    Cancelled,
    Sse { line: usize, reason: String },            // 流解析错误
    TruncatedToolArguments { call_index: usize },   // finish_reason=length 且参数不完整
}

pub struct ApiErrorBody {
    pub code: Option<String>,      // 实测可为 null
    pub message: Option<String>,
    pub param: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,      // invalid_request_error 等
}
```

## 4. 取消语义

- 网关方法签名携带 `CancellationToken`：`async fn chat(&self, req, cancel: CancellationToken) -> Result<...>`。
- 非流式：`tokio::select! { res = http.send() => ..., _ = cancel.cancelled() => return Err(Cancelled) }`；取消即 drop 掉 `Response`，reqwest 底层连接随之关闭/回池。
- 流式：对 `Response::bytes_stream()` 逐帧消费，用 `tokio::select!` 同时监听 `stream.next()` 与 `cancel.cancelled()`；取消分支 drop 掉 `Response`（`BytesStream` 持有 body 所有权），保证底层 TCP 连接被关闭/复用，不残留半读连接。
- SSE 分帧用 `BufRead`/按 `\n` 手动切分：reqwest 的 chunk 与 SSE 行不保证对齐，必须维护行缓冲，`[DONE]` 或取消时清空。
- 取消后：不落 `usage`（可能没到末块），上层把该 Turn 标记为 `cancelled`，预算按已消费估算值记账，不计为成功。

```rust
// 流式取消骨架（仅示意）
loop {
    tokio::select! {
        _ = cancel.cancelled() => return Err(GatewayError::Cancelled),
        frame = stream.next() => match frame {
            None => break,                          // 流自然结束
            Some(Err(e)) => return Err(Transport(e.to_string())),
            Some(Ok(bytes)) => feed_frame(&mut line_buf, &bytes, &mut acc)?,
        },
    }
}
```

## 5. 模型能力表

数据来自 `GET /v1/models` 实测（2026-11-23），`tool_call` 全部为 `true`。上下文长度是网关声明值，真实上游限额可能更低，实现时按「声明值」暴露并在超限前自设保守上限。

| 模型 id | tool_call | context_length | max_output_tokens | reasoning_levels | 默认路由 |
|---|---|---|---|---|---|
| `workbuddy/deepseek-v4.1-flash` | true | 1,000,000 | 128,000 | none/low/high/max | H07 主写 |
| `workbuddy/deepseek-v4-pro` | true | 1,000,000 | 128,000 | none/high/xhigh | 流式/重试语义段 |
| `workbuddy/kimi-k3.1-pro` | true | 1,000,000 | 32,000 | low/high/max | 独立审计（只读） |
| `workbuddy/glm-5.3` | true | 1,000,000 | 64,000 | low/high/max | Web/协议整合 |
| `workbuddy/mimo-v2.6-pro` | true | 1,000,000 | 128,000 | low/high/max | 平台/安全审查 |
| `workbuddy/minimax-m3.1-flash` | true | 1,000,000 | 128,000 | low/medium/high/xhigh/max | 快速草稿 |
| `auto/deepseek-v4.1-flash` | true | 1,000,000 | 128,000 | none/low/high/max | 自动路由组别名 |
| `auto/deepseek-v4-pro` | true | 1,000,000 | 128,000 | none/high/xhigh | 自动路由组别名 |
| `auto/kimi-k3.1-pro` | true | 1,000,000 | 32,000 | low/high/max | 自动路由组别名 |
| `auto/glm-5.3` | true | 1,000,000 | 64,000 | low/high/max | 自动路由组别名 |
| `auto/auto` | true | 256,000 | 32,000 | low/high/max | 网关自动选择 |

路由规则：`workbuddy/*` 是显式指定模型，`auto/*` 是路由组（由网关在组内选型）。Hive 默认路由按 WHITEBOARD 模型路由表绑定；**路由失败不得静默换模型**，fallback 必须产生事件（H07 runbook 验收项）。

## 6. token 计量与预算

- **权威计量**：非流式 `usage` 恒定存在；流式仅在末块出现 `usage`，因此流式预算以「估算先行 + 末块校准」。
- **usage 缺失时估算**：
  - 输入估算：`prompt_estimate = sum(text.len() / 4)`（中英混合粗略），或后续接入本地 tokenizer 精确化；有 `cache` 命中字段时优先用 `prompt_tokens` 口径。
  - 输出估算：`completion_estimate = acc.content.len() / 4 + acc.tool_calls.arguments.len() / 4`。
- **预算检查点**：
  1. 请求前：`estimated_prompt > budget_remaining` → 直接拒绝，不发请求。
  2. 流式中：每收到 N 个 delta（如每 64 个）比较 `estimated_consumed` 与预算，超限即 `cancel.cancel()` 中止。
  3. 请求后：用权威 `usage` 覆盖估算并落账；若权威值超限，标记 `budget_exceeded` 但结果仍可用（已产生则不丢弃，避免浪费）。
- **预算超限中止**：通过 `CancellationToken` 打断（见第 4 节），事件标记 `budget_exceeded`，不重试，不把超限当可重试错误。
- `usage.quota.percent`（实测流式末块提供）作为**软预警**：超过阈值（如 95%）在事件中告警，但不作为硬中止条件，因为其语义是网关配额而非项目预算。

## 7. H07 验收清单（可执行测试）

测试名以 `gateway_` 为前缀，`mock` 指本地 fake OpenAI/SSE 服务器（`hive-llm` 测试内 `hyper`/`axum` 起临时端口）。

1. `gateway_nonstream_ok` — 非流式 200 → 解析出 `content`、`finish_reason=Stop`、`usage.total_tokens` 且 `usage` 非空。
2. `gateway_nonstream_tool_call` — 带 tools 的 200 → 提取 `tool_calls[0].function.name/arguments`，`arguments` 可 `serde_json::from_str` 成功。
3. `gateway_bad_model_400_no_retry` — 模型不存在 → `GatewayError::Api{status:400}`，错误 `code=None` 不 panic，重试计数为 0。
4. `gateway_stream_concat` — mock SSE：`data:` 多帧内容 → 累积文本与逐块一致，`[DONE]` 后 `finish_reason=Stop` 收口。
5. `gateway_stream_usage_last_chunk` — mock SSE：中间 chunk `usage=null`，末块 `usage` 非空 → 解析器只在末块写 `usage`，前块不误判。
6. `gateway_stream_heartbeat_skip` — mock SSE：`[DONE]` 前插入 `: heartbeat` 注释行 → 被忽略，不影响结果。
7. `gateway_stream_tool_call_delta` — mock SSE：同一 `index` 的 `arguments` 分多帧 → `PendingToolCall.arguments` 按序拼接完整。
8. `gateway_truncated_tool_args` — `finish_reason=Length` 且 `arguments` 非合法 JSON → 返回 `TruncatedToolArguments`，不执行半截工具。
9. `gateway_retry_429_backoff` — mock 先 429 后 200 → 重试次数≥1 且 ≤上限，退避有上限，最终成功。
10. `gateway_retry_5xx_cap` — mock 连续 500 → 达到 `max_retries` 后返回错误，不无限重试。
11. `gateway_no_retry_on_4xx` — mock 401 → 不重试，返回鉴权错误，错误消息不含 key。
12. `gateway_cancel_stream` — mock SSE 慢速发送，`CancellationToken` 取消 → 流式在限定时间内返回 `Cancelled`，连接释放（服务器观察到断开）。
13. `gateway_timeout` — mock 延迟超过读超时 → 返回 `Timeout`，计入重试预算。
14. `gateway_budget_abort_stream` — 流式中估算超预算 → 触发取消，事件标记 `budget_exceeded`。

（14 条满足 H07 runbook 的 fake server 覆盖要求，其中 4/5/6/7/12 为 mock SSE 用例。）

## 8. 风险登记（最容易做错的 3 点）

1. **截断 tool call 参数被当成完整 JSON 执行**。实测 `max_tokens` 截断使 `arguments` 变成 `{"city"`，`finish_reason="length"`。若实现只做 `content` 拼接而忽略 `finish_reason`，会把非法 JSON 交给工具层，导致执行失败甚至半参数副作用。对策：`finish_reason=Length` 时对所有 `PendingToolCall.arguments` 做 JSON 校验，不合法则续写或判为截断失败，绝不直接执行。
2. **流式 `usage` 只在末块，中断即丢失权威计量**。取消、超时或断流发生在末块之前，`usage` 缺失；若预算/计费逻辑依赖权威 `usage` 而缺省为 0，会漏记账。对策：预算用估算先行，权威 `usage` 仅作校准；中断路径显式落「已估算、未确认」状态，不把 0 当真值。
3. **错误体 `code=null` 导致朴素 `code` 匹配崩溃或误分类**。实测 `{"code":null,"type":"invalid_request_error"}`；`unwrap()` 会 panic，按 `code` 字符串比较会漏掉 429/5xx。对策：`code` 用 `Option`，分类只依赖 HTTP 状态 + `type`，未识别 `type` 降级为通用错误并保留原始 body。

## 附：未改动声明

本任务只新增 `docs/design/H07-model-gateway.md`，未触碰 `crates/`、`WHITEBOARD.md`、其他 `docs/` 文件；未运行 cargo，未安装依赖；网络仅访问 `127.0.0.1:3425`，未访问外网。
