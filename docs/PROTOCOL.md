# Hive 协议契约

> 状态：H01 冻结候选
> 版本：0.1
> 任何破坏性变更必须通过 `docs/ADR/` 记录。

## 1. 标识符

所有公共标识符使用带前缀的 UUID v4：

```text
m_<uuid>    Mission
r_<uuid>    Run
n_<uuid>    Node
a_<uuid>    Agent Session
tn_<uuid>   Turn
e_<uuid>    Event
tc_<uuid>   Tool Call
ap_<uuid>   Approval
f_<uuid>    Artifact
prj_<uuid>  Project
tr_<uuid>   Trace
```

外部请求不得自行拼造内部 ID。

事件有两个不同职责的标识：

- `event_id`：`e_<uuid>`，稳定事件身份，用于幂等、引用和审计。
- `seq`：当前数据库内单调递增的整数，取值范围 `1..=2^53-1`，是唯一客户端游标和排序依据。上界同时满足 SQLite 有符号 `INTEGER` 与 JavaScript 安全整数；分配前必须检查，耗尽返回 `sequence_exhausted`，禁止回绕或饱和。

`seq` 不作为跨数据库时间或全局顺序。事件表以 `seq` 为主键，`event_id` 为唯一键；不得再把 `event_id` 用作自增整数。

## 2. 状态机

Mission（只持久化真实生命周期状态）：

```text
queued -> planning -> running -> done
                 |       |
                 |       +-> paused -> running
                 +-------+-> failed
                         +-> cancelled
```

`waiting_approval` **不是** Mission 的持久状态，见下方约束。

Node：

```text
pending -> ready -> leased -> running -> succeeded
                              |  |
                              |  +-> waiting_approval -> running
                              |  +-> failed -> retry_ready
                              |                 |  |
                              |                 |  +-> ready (attempt_no + 1)
                              |                 +----> failed_final (budget exhausted)
                              |  +-> needs_recovery
                              |        |  |
                              |        |  +-> ready (manual retry)
                              |        +----> failed_final (manual abort)
                              +-> cancelled
pending -> skipped
```

约束：

- 终态是 `succeeded`、`failed_final`、`cancelled`、`skipped`。
- `failed` 是当前 attempt 的瞬时失败态；是否重试由重试预算决定，重试必须进入 `retry_ready -> ready` 并产生新的 `attempt_no`。
- `needs_recovery` 表示副作用是否完成无法确认，必须人工选择重试或最终失败，不能自动重放。
- `attempt_no` 是逻辑执行次数，从 1 开始，非零；租约所有权使用独立的 `lease_token`，不得混用。数据类型为 `u32`，SQLite 存非负 `i64` 并做有检查转换，禁止溢出回绕。
- Node 级事件的 `attempt_no` 必填且非零；Mission 级事件没有 Node attempt，序列化为 `null`，**禁止用 0 冒充**。
- 父 Mission 或 Run 的取消必须在同一事务级联到所有未终态 Node，并取消其 pending 审批；Node 进入 `cancelled` 且撤销当前租约。
- Mission 的 `waiting_approval` 是读取层派生的观测态：仅当当前 Run 没有任何可运行 Node 且至少一个 Node 处于 `waiting_approval` 时呈现。单个 Node 等待审批不得阻塞其他分支，也不得写入 `missions.state`。
- `waiting_approval` 不是 Node 终态，审批恢复后回到原 Node 运行态。
- 服务重启后的状态恢复按 ADR 0002 执行；不得把所有未完成 Mission 简单标记为失败。

### 审批

审批状态：

```text
pending -> approved
        -> rejected
        -> expired
        -> cancelled
```

- 默认超时为 600 秒，超时进入 `expired`，工具调用按拒绝处理。
- `approved`、`rejected`、`expired`、`cancelled` 是终态；重复决定返回冲突错误。
- pending 审批持久化；服务重启后保留，不因重启自动批准或自动拒绝。
- 审批所属 Mission/Node 被取消时，pending 审批进入 `cancelled`。

Run：

```text
created -> planning -> running -> done
                    |          |
                    |          +-> paused -> running
                    |          +-> failed
                    |          +-> cancelled
                    +-> failed
                    +-> cancelled
```

Run 是 Mission 的一次规划/执行生命周期；追加要求或恢复可以在同一 Mission 下创建新 Run，但不得复用已终态 Run 的 Node 历史。

### 重试与租约

- Node 默认租约 60 秒，执行者至少每 20 秒续租。
- 租约过期后，新的执行者获得新的 `lease_token`；是否同时递增 `attempt_no` 取决于节点是否真正开始过：`leased` 过期退回 `ready`（未开始，attempt 不变），`running` 过期进入 `needs_recovery`（副作用不确定，必须人工裁决）。
- 所有事件、工具结果和产物提交必须同时满足两个条件，缺一不可：`(node_id, attempt_no, lease_token)` 与提交内容一致，且 Node 处于允许接收该提交的非终态。仅校验 token 或仅校验状态都不够。
- `retry_budget` 定义为**首次执行之后的额外执行次数**，默认 3，即最多 4 个 attempt，可由角色或 Mission 策略覆盖。持久层只保存 `attempt_no` 与解析后的 `retry_budget`，不保存会漂移的“剩余次数”；当前 attempt 失败时，若 `attempt_no <= retry_budget` 则进入 `retry_ready`，否则进入 `failed_final`。
- `attempt_no` 在下一次实际领取并开始执行的事务中递增，不在 `node.failed` 时预递增。
- `lease_owner` 是一次 runtime 调度实例的诊断标识，不是授权凭据；授权只认 `lease_token`。单进程内多个 task 共用同一 instance id，每次领取仍生成新 token；进程重启生成新 instance id，并撤销旧实例的租约。
- 模型网关单次请求内部的重试（429/5xx/断流）独立计数，不扣减 Node 的 `retry_budget`；网关不直接写 Node 状态或事件。
- 文件提交使用临时文件 + 原子重命名；Shell 等不可天然幂等的工具必须写入 `intent / started / completed` 事件。
- 恢复顺序：回收过期租约 -> 检查未完成副作用 -> 可安全重试则创建新 attempt -> 不确定则进入 `needs_recovery`。

### 快照

- 快照是按 `last_seq` 标记的**物化状态缓存**，不是独立权威，也不做事件压缩。
- 快照必须包含 `schema_version`、作用域（mission/run/node）、`last_seq` 与状态版本；写入与对应状态、事件在同一事务提交。
- 恢复以持久状态表 + 事件重放为准；仅当 `schema_version`、作用域与游标全部匹配且校验通过时，才把快照用作加速。任一不符立即弃用并重建。
- 首版不得因为存在快照而删除其后仍需的事件；事件保留策略由 H09 定义。

### 预算

- 预算字段与单位：token 用 `u64`，时间用毫秒整数，费用用带 `currency` 的最小货币单位整数。
- 每次 Turn 的 usage 必须标记 `estimated` 或 `authoritative`，并用请求 ID 幂等校准；断流或取消导致 usage 缺失时记 `estimated`，不得记 0。
- H05 拥有 Mission/Run/Node 的预算分配与聚合账本；H04 记录每 Turn usage；H07 只提供单次请求估算、权威 usage 与可取消调用，**不得**直接决定 Node/Mission 状态。
- 预算超限是 Node 的结构化失败原因（`budget_exceeded`），不是新的 Node 状态，也不自动重试。

## 3. 事件封套

```json
{
  "schema_version": 1,
  "event_id": "e_...",
  "seq": 42,
  "kind": "node.succeeded",
  "mission_id": "m_...",
  "run_id": "r_...",
  "node_id": "n_...",
  "session_id": null,
  "attempt_no": 1,
  "created_at": "2026-11-23T00:00:00Z",
  "trace_id": "tr_...",
  "payload": {}
}
```

规则：

- 状态转移与对应事件在同一事务中提交。
- 客户端使用 `seq` 游标补齐；SSE 断线不能导致状态永久缺失。
- 事件持久化内容为权威版本；内存广播可以丢弃旧事件，但不能丢持久化记录。
- `schema_version` 必须可向后兼容读取；新增字段默认可选。
- 工具完整输出可以单独存储，事件只放摘要和引用。
- 持久化封套**不含** `agent` 字段；`agent` 是读取/SSE 层按 `session_id` 投影出的展示字段（见 §6），不得写进 events 表，避免同一事件出现两个身份来源。
- `run_id`、`node_id`、`session_id` 在 Mission 级事件上允许为 `null`；读取方必须按可空处理。

## 4. 首版事件族

| kind | 语义 |
|---|---|
| `mission.created` | Mission 创建 |
| `mission.started` | Mission 开始执行 |
| `plan.created` | DAG 计划写入 |
| `plan.revised` | 运行时重新规划 |
| `plan.degraded` | 规划器失败，使用降级计划 |
| `node.ready` | 依赖满足，可调度 |
| `node.started` | 节点领取并开始 |
| `node.turn` | Agent Turn 开始 |
| `node.thought` | Agent 的简短判断或思考摘要 |
| `node.tool_call` | 工具调用意图 |
| `node.tool_started` | 工具副作用实际发生（OS spawn / connect 成功），用于崩溃恢复判定 |
| `node.tool_result` | 工具结果摘要 |
| `node.approval_requested` | 需要人工审批 |
| `node.approval_decided` | 审批结果 |
| `node.succeeded` | 节点成功 |
| `node.failed` | 节点失败 |
| `node.budget_exceeded` | 节点预算超限导致的失败 |
| `node.cancelled` | 节点取消 |
| `node.skipped` | 节点跳过 |
| `mission.summary` | 最终总结 |
| `mission.done` | Mission 成功 |
| `mission.failed` | Mission 失败 |
| `mission.cancelled` | Mission 取消 |
| `message.user` | 用户追加要求 |
| `routine.created` | 定时任务创建 |
| `routine.updated` | 定时任务启停或配置更新 |
| `routine.deleted` | 定时任务删除 |
| `routine.triggered` | 定时任务触发 Mission |
| `routine.error` | 定时表达式或触发失败 |
| `artifact.saved` | 产物登记 |
| `model.retry` | 网关单次请求内部重试 |
| `model.fallback` | 网关切换到备用模型 |

`agent.thought` 统一改为 `node.thought`。追加要求产生 `message.user`，随后由新 Run 产生 `plan.created` 或 `plan.revised`。事件 payload 的精确 JSON schema 在 H02 随实现冻结；新增事件不得改变既有事件语义。

`node.tool_call` 只表示 intent，`node.tool_started` 表示副作用已发生，`node.tool_result` 承载 completed/failed/cancelled 结果。三者缺一不可，否则崩溃恢复无法区分“没执行”和“执行了但不知道结果”。runbook 里出现的裸词不得当作隐式协议，必须先进本表。

## 5. Handoff v1

```json
{
  "schema_version": 1,
  "mission_id": "m_...",
  "run_id": "r_...",
  "node_id": "n_...",
  "agent": "scout",
  "objective": "本节点目标",
  "summary": "给下游的简短结论",
  "inputs": [],
  "artifacts": [
    {"id": "f_...", "path": "artifacts/report.md", "sha256": "..."}
  ],
  "decisions": [
    {"title": "选择", "rationale": "原因"}
  ],
  "assumptions": [],
  "risks": [],
  "open_questions": [],
  "acceptance": ["可检查的完成条件"],
  "recommended_next": [],
  "provenance": {
    "attempt_no": 1,
    "base_revision": "git-sha-or-content-hash",
    "created_at": "2026-11-23T00:00:00Z"
  }
}
```

Handoff 不得包含完整模型历史、完整工具日志、密钥、Cookie 或未脱敏的个人信息。

## 6. API 兼容策略

- Web 继续使用 HTTP/SSE；首版路径保持 `/api/*`，但事件内容按本协议升级。
- 首版必须保留以下路径：`/api/health`、`/api/state`、`/api/missions`、`/api/missions/{id}`、`/api/missions/{id}/messages`、`/api/artifacts/{id}`、`/api/events`、`/api/approvals/{id}`、`/api/routines`、`/api/routines/{id}`。
- 新 SSE 事件外层为 `{schema_version, seq, event_id, mission_id, run_id, node_id, session_id, agent, attempt_no, created_at, trace_id, kind, payload}`；旧前端的 `task_id/agent/content` 由 H08 更新，不保证旧前端不经修改即可运行。
- 桌面/IDE 接入优先新增 JSON-RPC over stdio，不复制业务逻辑。
- 所有写请求要求明确的内容类型和大小限制。
- 错误响应包含稳定的 `code`、`message`、可选 `details`，不回传内部堆栈。
- 首版不承诺跨大版本协议兼容；小版本新增字段必须可选。

## 7. 工作区协议

- `.hive/`：数据库、运行状态、策略与迁移；不得作为用户产物。
- Project：注册到 Hive 的项目工作区根，使用 `prj_` 标识；Mission 可以绑定一个 Project，也可以是无项目的临时 Mission。
- `work/`：Agent 临时文件。
- `outputs/`：最终交付物。
- 代码写入阶段使用 worktree 或独立目录；集成前不得宣称已合并。

## 8. 安全承诺

- 网络请求：先解析并对所有地址做公网校验，再连接已校验的具体 IP；禁止自动跟随重定向，或对每一跳重新执行完整校验。未实现连接级绑定时必须标注限制，不能宣称已防 DNS rebinding。
- 路径：Windows 必须测试短路径、junction、symlink、UNC、驱动器相对路径和大小写变体；不能只依赖字符串前缀判断。
- Shell：默认关闭；启用后仍需逐次审批。OS 级隔离未实现前，工具能力只能标注为“应用层限制”。
- 所有工具调用必须经过统一 `policy -> approval -> execute -> audit` 路径。

## 9. 数据迁移

- 首版不自动迁移 Python SQLite 数据库。
- 可提供一次性导出/导入器，只导入 Mission 目标、消息和已生成产物元数据；旧 tasks/events 不映射为新的 Run/Node 事件。
- 迁移前必须备份旧数据库；迁移失败不得修改源库。
- H11 必须提供明确的“不迁移”说明和旧数据导出方案，不得等到发布时再决定。

## 10. 数据表归属与迁移链

ID 已存在不代表表已存在。需要当前状态、原子 CAS、重启查询或文件元数据的实体必须建表；只有通知性质的信息可以只走事件流。序号即迁移顺序，不得插队或复用。

| 实体 | 最早阶段 | 承载方式 |
|---|---|---|
| `missions`、`runs`、`nodes`、`events`、`snapshots`、`approvals` | H02 | 表（V1） |
| `tool_calls`、`artifacts`、`tool_audit` | H03 | 表（V2） |
| `agent_sessions`、`turns`、`turn_usage` | H04 | 表（V3） |
| `budgets` | H05 | 表（V4） |
| `handoffs` | H06 | 表（V5） |
| `routines` | H09 | 表（V6） |

| 迁移 | 阶段 | 内容 |
|---|---|---|
| V1 | H02 | Mission/Run/Node/Event/Snapshot/Approval |
| V2 | H03 | ToolCall、Artifact、ToolAudit（哈希链） |
| V3 | H04 | AgentSession、Turn、TurnUsage |
| V4 | H05 | 预算分配与账本 |
| V5 | H06 | Handoff |
| V6 | H09 | Routine 与保留策略 |

### V3 列契约（H04 冻结）

`agent_sessions`：`session_id` 主键，`mission_id`/`run_id`/`node_id` 外键，`role`、`model`、`system_prompt`，`state ∈ {active, completed, failed, cancelled}`，`created_at_ms`/`updated_at_ms`/`closed_at_ms`。

`turns`：`turn_id` 主键，`session_id`/`node_id` 外键，`attempt_no`、`turn_index`、`request_model`、`finish_reason`（可空）、`state ∈ {started, completed, failed, cancelled}`、`output_preview`、`tool_call_count`、时间戳；`UNIQUE(session_id, turn_index)`。

`turn_usage`：`turn_id` 主键并 1:1 关联 `turns`，`session_id` 外键，`prompt_tokens`/`completion_tokens`/`total_tokens`，`source ∈ {estimated, authoritative}`，`request_id`（可空），`created_at_ms`。

规则：

- `agent_sessions` 进入终态后必须写 `closed_at_ms`，同一 session 不得复活；续跑要新建 session。
- `turns` 的 `output_preview` 只放有界摘要，完整输出走 artifact 或工具结果引用。
- `turn_usage` 与 `turns` 一一对应；usage 缺失时写 `estimated`，不得写 0 冒充权威值。
- 行是当前状态，`node.turn` / `node.thought` 等事件是通知；两者在同一事务提交。

迁移规则：

- 数据库 schema 版本的唯一来源是 `hive-store` 的 `SCHEMA_VERSION`，与 `EVENT_SCHEMA_VERSION`、`HANDOFF_SCHEMA_VERSION` 无关；`hive-types` 不再导出通用 `SCHEMA_VERSION`。
- 迁移链只追加。V1 的 SQL 一旦合入即冻结，后续阶段不得修改 V1 来让“最新空库”测试变绿。
- 每个迁移在独立事务中执行，成功后写 `PRAGMA user_version`；版本高于代码支持时直接拒绝启动。
- 新增迁移的验收必须同时覆盖 `fresh -> 最新` 与 `上一版 -> 最新`，并原样重跑既有迁移与恢复测试。
- 在对应表出现之前，事件里的 `session_id`、`artifact_id` 等字段只是不设外键的标识，不得声明 FK。

## 11. H01 验收

H01 必须提供类型与配置 crate 的最小实现，并在工具链安装后通过：

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p hive-cli -- doctor
```

没有 Rust 工具链时，只能标记为 `implemented, unverified`，不能标记完成。

