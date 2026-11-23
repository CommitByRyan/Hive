# H08 — HTTP/SSE 协议与 Web 控制台设计

> 状态：design ready；依赖 H05 调度实现与 H07 模型网关，尚未实现。
>
> 结论：HTTP 只做服务边界，不做状态所有者。客户端先取带 `last_seq` 的快照，再用 `seq` 订阅增量；事件持久层是权威，SSE 只是实时投影。慢客户端必须被有界丢弃并转入游标补偿，绝不阻塞 SQLite 单写者或 DAG 调度。

## 1. 路由表

统一约定：

- 成功响应：`200`，创建用 `201`；`Content-Type: application/json; charset=utf-8`；`Cache-Control: no-store`。
- 错误响应：`{"error":{"code":"...","message":"...","details":{}}}`，不回传内部堆栈。
- 写请求：必须 `Content-Type: application/json`，且正文上限 64 KiB；超过即 `413 payload_too_large`。
- ID：路径参数必须符合现有前缀 UUID；格式非法返回 `400 invalid_id`。
- 时间：HTTP 层输出 RFC 3339；存储层继续使用毫秒时间戳，不改变 schema。

| 方法与路径 | 请求体 | 成功响应 | 错误码 |
|---|---|---|---|
| `GET /api/health` | — | `{"ok":true,"schema_version":1,"mode":"live|demo","model_catalog_ready":true}` | `500 internal_error` |
| `GET /api/state` | — | `{"schema_version":1,"last_seq":N,"config":{...},"missions":[...],"approvals":[...],"stats":{...}}` | `500` |
| `GET /api/missions` | query：`limit?`（默认 60，上限 200）、`offset?` | `{"missions":[...],"last_seq":N}` | `400 invalid_request`、`500` |
| `POST /api/missions` | `{"title":"...","goal":"..."}`；`title` 可选、`goal` 必填且 trim 后 1..=16 KiB | `201 {"mission":{...,"last_seq":N}}` | `400 invalid_request`、`409 mission_conflict`、`413`、`500` |
| `GET /api/missions/{id}` | — | `{"mission":{...},"runs":[...],"nodes":[...],"events":[...],"artifacts":[...],"messages":[...],"approvals":[...],"last_seq":N}` | `400 invalid_id`、`404 not_found`、`500` |
| `POST /api/missions/{id}/messages` | `{"content":"..."}`，trim 后 1..=16 KiB | `201 {"message":{...},"mission":{...,"last_seq":N}}` | `400`、`404`、`409 mission_not_accepting_messages`、`413`、`500` |
| `GET /api/events` | query：`mission_id?`、`after_seq?`；`Accept: text/event-stream` | SSE，见 §2 | `400 invalid_seq`、`400 invalid_id`、`404 mission_not_found`、`406 unsupported_media_type` |
| `POST /api/approvals/{id}` | `{"decision":"approve|reject","note?":"..."}`；`note` ≤ 4 KiB | `200 {"approval":{...},"node":{...},"last_seq":N}` | `400 invalid_request`、`400 approval_not_pending`、`400 approval_expired`、`404`、`409 approval_conflict`、`413`、`500` |
| `GET /api/artifacts/{id}` | query：`download?` | 元数据 + 文本内容或文件流 | `404 artifact_not_found`、`410 artifact_missing`、`415 artifact_not_renderable`、`500` |
| `GET /api/routines` | — | `{"routines":[...]}`；H09 前恒为空数组 | `500` |
| `POST /api/routines` | H09 定义；H08 预留 | `201 {"routine":{...}}` | `400 not_available_before_h09`、`500` |

说明：

- `POST /api/routines/{id}`、`PATCH`、`DELETE` 不在用户指定的首版清单内；H09 若需要，应新增子路径设计而不是把语义塞进本文件。
- `GET /api/missions/{id}` 分页：`events_after_seq`、`events_limit`（默认 500，上限 2000）、`runs_limit`、`nodes_limit`；事件必须升序。快照先返回，事件只补到响应生成时的 `last_seq`。
- 响应中的 `mission` 含 `state`（持久状态）与 `observation`（派生状态）。UI 主显 `observation`，审计视图显示 `state`。
- `model_catalog_ready` 只说明 `ModelCatalog` 已成功加载且至少一个模型可用，不暴露 API Key、base URL 或完整模型清单。
- `approvals` 只返回未消费且未过期的 pending 记录；历史审批走 Mission 详情的事件与审批查询。

## 2. SSE 协议

连接流程：

1. 客户端先取快照（`/api/state` 或 `/api/missions/{id}`），记录其 `last_seq`，立即渲染。
2. 建立 `GET /api/events?after_seq={last_seq}`；如果只看一个 Mission，加 `mission_id` 以减少广播和补偿量。
3. 服务端校验 `after_seq ∈ 0..=2^53-1`，并查询持久事件，从 `seq > after_seq` 开始按升序补发。
4. 补发完成后进入实时推送；每次收到事件，`id:` 写该事件 `seq`，`event:` 写 `kind`，`data:` 写协议封套 JSON。
5. 浏览器 `EventSource` 自动重连时回传 `Last-Event-ID`。服务端优先使用 `Last-Event-ID`，同时提供 `?after_seq=` 供测试工具和手动恢复使用；两者都出现且不一致时返回 `400 invalid_cursor`。
6. 重连后服务端重复第 3 步，从持久层补齐，不依赖内存队列中还保留旧事件。

帧示例：

```text
retry: 2500

id: 42
event: node.approval_requested
data: {"schema_version":1,"seq":42,"event_id":"e_...","mission_id":"m_...","run_id":"r_...","node_id":"n_...","session_id":"s_...","agent":"builder","attempt_no":1,"created_at":"...","trace_id":"tr_...","kind":"node.approval_requested","payload":{}}

: heartbeat
```

参数：

- `retry`：2500 ms。
- 心跳：无业务事件时每 20 秒发送 `: heartbeat\n\n`，并覆盖代理与浏览器空闲超时。
- 订阅队列：每个 SSE 连接一个 256 条的内存队列；这就是单连接最大未确认缓冲数。
- 慢客户端：队列满时丢最旧事件并增加 lag 计数；不阻塞事件提交、SQLite 写事务或调度器。
- lag 通知：当 lag > 0 时，先发送专用控制事件：

```text
event: hive.lagged
data: {"lagged":N,"last_persisted_seq":M}
```

客户端收到后停止归约，标记 UI 为“事件落后”，调用 `GET /api/missions/{id}?events_after_seq={本地 last_seq}` 补齐；补齐成功后继续消费实时队列。服务端不能假设客户端一定能消费该控制事件，因此补齐路径始终以持久层为权威。
- 序列回退：本地 `last_seq` 大于服务器 `latest_seq` 时返回 `409 sequence_ahead`；客户端必须重新取完整快照，不能盲目归约。
- 非浏览器客户端可以不使用 `Accept: text/event-stream`，但 H08 对 `GET /api/events` 强制要求该 Accept，避免普通 GET 被代理误当 JSON API。

## 3. 状态一致性

快照 + 增量的读取规则：

1. 快照在一个只读事务中读取 mission、runs、nodes、approvals、统计和事件游标，响应必须携带该事务看到的 `last_seq`。
2. SSE 从 `after_seq=last_seq` 开始补偿。若在快照事务提交后、SSE 建立前有新事件，持久层补偿会覆盖这段窗口。
3. 客户端只按 `seq` 单调归约：`new.seq > local_last_seq` 才应用；相等跳过；小于则忽略为延迟重复；本地游标大于服务器最新值则触发全量快照。
4. `/api/state` 的 `missions` 列表最多 60 条，按 `updated_at_ms` 降序；每条都带 `last_seq`。切换详情页时仍必须取 Mission 快照，不能把列表摘要当完整事实。
5. 事件归约只用于优化即时 UI；页面重载、恢复 lag、断线超过阈值或检测到游标异常时，重新取快照并丢弃本地派生差异。

`MissionObservation` 派生：

- 从 `MissionAggregate` 取 Mission 持久状态、全部 Node 状态与 pending 审批。
- `has_runnable_node = 任一 Node ∈ {pending, ready, leased, running, retry_ready}`。
- `has_pending_approval = 任一 approval state=pending 且未过期`。
- 调用现有 `observe_mission(state, has_runnable_node, has_pending_approval)`。
- `waiting_approval` 只有在 Mission 为 `running`、无可运行 Node、且存在 pending 审批时出现；其他状态一一映射持久状态。该值只写入响应的 `observation`，不得写回 `missions.state`。

SSE 对审批显示的归约：`node.approval_requested` 添加 pending 卡片；`node.approval_decided` 按 `event_id/approval_id` 幂等更新结果；`node.succeeded/failed/cancelled` 清除该节点的可操作状态。归约器遇到未知事件 kind 时保留数据但不猜测业务含义，并在 UI 事件流中原样展示。

## 4. 审批流

状态与可见性：

- 工具策略判定需要审批时，Runtime 在同一事务中把 Node 置为 `waiting_approval`、写入 approval 行和 `node.approval_requested` 事件；SSE 与 `/api/state` 都能看到该 pending。
- 审批卡片展示：目标 Mission、Node、工具名、参数摘要、`request_hash` 前 12 位、策略版本、attempt、过期倒计时；不展示 lease token。
- 决定请求使用原子 `Store::decide_approval`。并发时只有一个请求成功，失败请求得到明确错误。
- 重复决定：若当前状态已经是 `approved/rejected/expired`，返回 `400 approval_not_pending`；若决定发生在过期边界，返回 `400 approval_expired`。两者都不改变已有状态。
- 成功决定后 Runtime 消费审批并恢复工具执行；消费由 tool call、request hash、attempt 和 lease token 全量校验，防止旧 attempt 或旧租约复用审批。

超时：

- 过期检查由 Runtime 周期任务调用 `expire_overdue_approvals`；客户端不负责判定权威过期。
- 过期后 UI 将卡片从“可操作”改为“已过期”，保留节点与事件上下文，按钮禁用，并提示该工具不会执行。
- 客户端倒计时只用于提醒；在剩余 10 秒内提交仍以服务端事务判定为准。若服务端已过期，返回 `approval_expired` 后 UI 同步该事实。

## 5. 最小控制台信息架构

布局：

```text
┌───────────────┬────────────────────────────┬──────────────────┐
│ Mission 列表   │ Mission 概览 / DAG 视图      │ 审批面板          │
│ 创建 / 搜索    │ 事件流（默认折叠过程细节）     │ 产物列表          │
│ 观察状态过滤    │ 追加消息输入                 │ 模型能力摘要       │
└───────────────┴────────────────────────────┴──────────────────┘
```

- Mission 列表：标题、目标摘要、`observation`、更新时间、active 审批数；点击加载详情并切换 SSE 过滤。
- DAG 视图：Node 为卡片，边表示依赖；按 `run_id` 分层，展示 state、attempt、等待审批与失败原因。首次渲染使用快照 nodes；后续只更新受影响节点，不重绘整图。
- 事件流：按 `seq` 升序追加；工具过程默认折叠，审批、失败、预算、模型 fallback 展开显示。用户滚到底部时自动跟随，否则保持滚动位置。
- 审批面板：全局 pending 审批；未打开对应 Mission 时也可见，点击定位到节点。
- 产物列表：来自 Mission 快照与 `artifact.saved`；点击调用 artifacts API；非文本产物显示下载与类型提示。

状态管理：

- 单一前端 store：`{snapshot, events, eventIndex, approvals, artifacts, localLastSeq, connection, selection}`。
- 事件先入 `eventIndex: Map<seq,event>`，再按白名单归约到 mission/run/node/approval/artifact 投影；渲染只读投影，不直接改 DOM 状态。
- 所有写请求完成后，以响应中的 `last_seq` 为下限触发补偿，而不是信任“写成功后 UI 本地直接改状态”。
- 不整页轮询。SSE 正常时不轮询；断线重连期间显示连接状态；连续 3 次重连失败后允许 30 秒一次的低频快照兜底。

## 6. 安全

- 绑定：默认仅监听 `127.0.0.1`；配置为非 loopback 时启动日志与健康检查必须标记 `authentication_required=true`，H08 本身仍不提供账号体系。
- Origin/CSRF：所有 `/api/*` 写请求要求 Origin 为空（本地工具）或匹配本地控制台白名单；不匹配返回 `403 origin_forbidden`。SSE 是 GET，不执行写操作，但仍校验 Origin，避免第三方页面读取本地数据。
- 内容类型与大小：写请求必须 JSON，64 KiB 上限，超限 `413`；URL 长度上限 2 KiB；查询参数只接受白名单键，未知键返回 `400 unknown_parameter`。
- 静态资源：只从内置 Web 目录白名单解析，拒绝 `..`、绝对路径、UNC、junction/symlink；设置 `X-Content-Type-Options: nosniff`，静态响应可缓存短版本，API 不缓存。
- 身份降级：无认证模式下 `decided_by` 固定为 `local_console`，并在状态与健康响应声明 `approver_identity:"unauthenticated_local_console"`。不能宣称审批人身份真实可信；日志只记录来源地址与请求时间作为辅助审计。
- 凭据：任何 API 响应不得暴露 API Key、Cookie、环境变量、base URL 或模型请求头。模型能力摘要只暴露渲染必需字段。

## 7. 测试清单

以下均为可执行断言，建议放在 `hive-server` 集成测试与前端最小 E2E 测试：

1. `GET /api/health` 返回 `ok=true`、`schema_version=1`，且不含 API Key、base URL。
2. `POST /api/missions` 空 goal、超长 goal、非 JSON 分别返回 `400/413/400`，不创建事件。
3. `GET /api/missions/{id}` 返回 mission/runs/nodes 与 `last_seq=N`，且事件全部 `seq <= N`、升序。
4. SSE 以 `after_seq=N` 连接后，先收到 `seq=N+1..M` 的持久补偿，再收到实时事件；帧 `id` 与 JSON `seq` 一致。
5. 断开 SSE 并在期间写入 300 条事件，重连带 `Last-Event-ID=N` 能补齐 `N+1..N+300`，不依赖内存缓冲。
6. 慢客户端队列容量 256：发布 300 条事件时不阻塞写线程，`latest_seq` 达到 300，客户端收到 `hive.lagged`，随后可通过查询补偿。
7. 并发 20 个客户端同时请求并追加事件：所有订阅者最终能从持久层补偿到同一 `latest_seq`；服务端写路径无死锁。
8. 并发 10 个请求决定同一 pending 审批：恰好一个返回 200，其余返回 `approval_not_pending` 或 `approval_conflict`；最终状态唯一。
9. 第二次决定同一审批返回 `400 approval_not_pending`，响应包含稳定 code，不改变状态。
10. 在 `expires_at_ms` 边界提交：服务端过期者得到 `approval_expired`；过期扫描后卡片状态为 `expired` 且 Node 不执行工具。
11. `after_seq=-1`、非数字、`2^53`、`Last-Event-ID` 与 query 不一致均返回 `400 invalid_seq/invalid_cursor`。
12. 65 KiB 写请求返回 `413 payload_too_large`，且连接关闭前不读取超过限制的正文。
13. 恶意 Origin 对 GET `/api/state`、GET `/api/events`、POST mission 分别返回 `403 origin_forbidden`。
14. 构造 Mission：一个 Node waiting approval，其他分支 running；`observation` 仍为 `running`，审批卡片可见，证明单个审批不阻塞其他分支。
15. 前端重载后先渲染快照，再接收增量；事件流滚动位置在非底部用户操作时不跳动。
16. 构造 `localLastSeq > latest_seq`，前端收到 `409 sequence_ahead` 后重新拉完整快照，不应用旧增量。

## 8. 风险登记

1. **把 SSE 当状态权威**：如果只消费内存广播，重启、lag 丢弃或订阅间隙都会丢状态。必须坚持“快照 + 持久 seq 补偿”，SSE 只负责降低延迟。
2. **审批时序竞态**：决定成功不等于工具能执行；旧 attempt/lease 或过期边界都可能使消费失败。UI 必须把决定与执行结果分开呈现，不能在 POST 成功后立即显示“已执行”。
3. **HTTP 层泄漏能力与凭据**：模型目录、配置和审批详情都很容易被顺手全量输出。H08 只能返回 UI 必需字段，并把无认证模式的审批人身份降级声明放在显眼位置。

## 9. Handoff

**状态**：设计完成，未实现。H08 仍依赖 H05 与 H07；本文件不修改冻结协议、schema、事件或状态机。

**改动文件**：仅新建 `docs/design/H08-http-console.md`。

**验证证据**：只读检查了 `WHITEBOARD.md`、`AGENTS.md`、`docs/PROTOCOL.md`、`docs/ARCHITECTURE.md`、`docs/runbooks/H08.md`、`docs/reference/HIVE_BUILD_SPEC.md` §9–10、`crates/hive-llm/src/catalog.rs`、`crates/hive-events/src/lib.rs`、`crates/hive-store/src/store.rs`、`crates/hive-store/src/records.rs`、`crates/hive-types/src/lib.rs`。未运行 cargo。

**遗留风险**：H05 尚未合并，Mission/Run/Node 的最终调度字段与失败原因 payload 可能在实现时补齐；Routine 归 H09，本文件只预留空列表；静态资源与前端渲染细节需在实现前做一次 UI 原型验证。

**建议下一步**：待 H05 合并后，由 H00 裁决 H08 文档；实现顺序建议为 API DTO 与错误映射 → 快照查询 → SSE 补偿/lag → 审批操作 → 控制台 store → E2E。
