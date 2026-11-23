# ADR 0005：预算、Routine 与启动恢复的契约补充

- 状态：accepted for H09a / H09b
- 日期：2026-11-23
- 输入：`docs/design/H09-recovery-observability.md` §7 的 8 条候选，以及 H00 对执行器恢复能力的复核
- 影响：`PROTOCOL.md` §1/§2/§4、`hive-types`、`hive-store` V5/V6、`hive-runtime::recovery`、`hive-server`

## 背景

H09 设计提出预算账本、Routine、启动恢复与可观测性。复核发现三处必须由契约所有者裁定：

1. `waiting_approval` 的“再驱动”被写成“重装 session 继续执行”，但当前 `AgentRunner` 每轮从 `system + goal` 重建消息，不加载历史 Turn，也没有工具结果重放保护，无法保证不重复副作用。
2. `running -> needs_recovery` 的恢复转移目前不产生事件，违反“状态转移与事件同事务提交”的可观测要求。
3. Routine 需要新的公开 ID 前缀和事件族，否则实现只能拼内部 ID 或复用 `r_`（已属于 Run）。

## 决策

| 编号 | 决策 | 状态 |
|---|---|---|
| D1 | Routine 使用 `rt_<uuid>` 前缀，`ID_PREFIXES` 扩到 13 项 | accepted |
| D2 | 启动时无条件撤销上一实例的全部存活租约 | accepted |
| D3 | 新增 `node.needs_recovery` 事件，所有进入该状态的转移必须发事件 | accepted |
| D4 | H09a 不做审批后的 session 续跑；孤儿审批按 fail-closed 处置 | accepted |
| D5 | 健康检查采用兼容追加：保留 `/api/health` 字段，新增 `/api/health/live`、`/api/health/ready` | accepted |
| D6 | `wall_ms` 默认软限；可选硬 `max_wall_ms` 仅在无 `started` 副作用时可强停 | accepted |
| D7 | `tool_audit` 活跃期保持全局链；归档后按 Mission 段独立 genesis 校验 | accepted |
| D8 | v1 时区固定 `Asia/Shanghai`，schema 保留 `timezone` 列但拒绝其他值 | accepted |
| D9 | 可观测性默认不引入 `tracing`/`metrics` 等新依赖，用内部最小实现 | accepted |
| D10 | 不新增 `missions.source` 列；Routine 与 Mission 的关联只走 `routine_fires.mission_id` | accepted |
| D11 | 新增 `routine.triggered/error/updated/deleted` 四个事件 kind | accepted |

### D1 Routine ID

`PROTOCOL.md §1` 增加：

```text
rt_<uuid>   Routine
```

`hive-types::ID_PREFIXES` 由 12 项扩到 13 项。H09b 实现，H09a 不改；Routine 行不得用内部自增或复用 `r_`。

### D2 启动租约撤销

新增 `recovery::revoke_all_active_leases(now_ms)`，在调度器接受新工作之前执行：

- `leased -> ready`：attempt 不变，清空租约，发 `node.ready`。
- `running -> needs_recovery`：清空租约，发 `node.needs_recovery`，不发 `node.failed`。

周期 tick 继续使用 `reclaim_expired_leases` 的“仅过期”语义；启动路径不得依赖 60s TTL 已过。两条路径共享单节点回收 helper，避免两套状态转移。

### D3 `node.needs_recovery` 事件

新增 `EventKind::NodeNeedsRecovery`，字符串 `node.needs_recovery`。payload 最小字段：

```json
{"reason":"lease_revoked_on_startup|lease_expired|approval_orphaned","last_state":"running|waiting_approval"}
```

所有 `* -> needs_recovery` 的转移必须在同一事务写该事件。H09a 更新 `EventKind` 的 `as_str/parse`、PROTOCOL §4 与相关测试。

### D4 孤儿审批：fail-closed

H09a 明确**不实现**“审批后重装 session 继续执行”。当前执行器不持久化逐 Turn 消息与工具结果，无法证明不会重放副作用。启动扫描对每个 `waiting_approval` 节点执行：

1. 同事务把 pending 或 approved-未消费的审批置为 `expired`：`decided_by='recovery'`、`decision_note='owner_lost'`，发 `node.approval_decided`（`decision='expired'`，`reason='owner_lost'`）。
2. 当前 attempt 没有 `started`/`completed` 副作用：
   - `waiting_approval -> failed`（reason `approval_orphaned`），发 `node.failed`；
   - 按既有重试预算走 `retry_ready -> ready`（attempt+1）或 `failed_final`。
3. 当前 attempt 已有 `started`/`completed` 副作用：
   - `waiting_approval -> needs_recovery`，发 `node.needs_recovery`；
   - 等待人工 `retry_node`/`abort_node`，绝不自动重放。

为此 `NodeState` 允许 `waiting_approval -> needs_recovery`。协议状态图与约束同步更新。

真正的“审批后续跑”留到后续阶段（H09c/H12），前置条件是：逐 Turn 消息持久化、工具结果按 `tool_call_id` 缓存、重放测试证明副作用调用次数不增加。未满足前不得在 UI 或文档宣称支持。

### D5 健康检查形状

保留 H08 已冻结字段：`ok`、`schema_version`、`event_schema_version`、`mode`、`model_catalog_ready`。H09b 追加可选字段 `status`、`checks`、`budget`，并新增：

- `GET /api/health/live`：liveness，失败 503。
- `GET /api/health/ready`：readiness，失败 503。

`degraded` 仍返回 200，不把可服务状态误报为故障。H08 的 11 条路由继续有效。

### D6 墙钟预算

`wall_ms` 默认只告警，不中断已发生的副作用。可选 `max_wall_ms` 仅在当前 attempt 尚无 `node.tool_started` 时允许强停并进入预算失败；一旦存在 `started`，降级为 `node.budget_exceeded` 告警并等待人工取消。

### D7 审计链归档

活跃数据继续使用全局 `tool_audit` 单链，`verify_tool_audit_chain` 行为不变。归档只针对终态 Mission 整段：写 `segment_genesis_hash`，按 Mission 段校验。不得重写 `seq`，不得部分删除活跃 Mission 事件；清理后发布 `retained_since_seq`。

### D8 时区

v1 只接受 `Asia/Shanghai`。`routines.timezone` 列保留用于演进，但创建/更新时其他值返回 `400 invalid_timezone`。`daily:HH:MM` 按该时区换算成 UTC epoch 毫秒存 `next_run_ms`。

### D9 观测依赖

默认用内部 JSON Lines logger 与内存指标注册表，不新增 crate。若后续引入 `tracing`/`metrics`，另提 ADR；不得在 H09b 静默加依赖。

### D10 Routine 与 Mission 关联

`missions` 表不新增 `source` 列。Routine 触发的 Mission 通过 `routine_fires(routine_id, fire_at_ms, mission_id)` 关联；`routine.triggered` payload 带 `routine_id`、`fire_at_ms`、`mission_id`。Mission 列表如需标记来源，由服务层 join 得出，不落冗余列。

### D11 Routine 事件族

新增四个事件：

| kind | 用途 | payload 最小字段 |
|---|---|---|
| `routine.triggered` | 到点并创建 Mission | `routine_id`、`fire_at_ms`、`mission_id` |
| `routine.error` | 解析/触发失败，可能停用 | `routine_id`、`code`、`message` |
| `routine.updated` | 配置更新 | `routine_id`、`changed` |
| `routine.deleted` | 删除 | `routine_id` |

`routine.*` 事件不属于 Node 生命周期，`node_id`/`attempt_no` 为 null；不得强行塞进 Node 事件流。

## 迁移与文件

- V4 = H06 Handoff，保持不动。
- V5 = H09a 预算账本；V6 = H09b Routine 与保留策略。
- V1–V4 SQL 不得修改；V5/V6 的列契约由 H09a/H09b 在实现时补入 `PROTOCOL.md §10` 并冻结。
- 本 ADR 先更新 `PROTOCOL.md §1/§2/§4`；`hive-types` 的代码变更由 H09a/H09b 按阶段实施，避免与 H06 并行写冲突。

## 验收

- `waiting_approval` 的三种启动分支各有测试：无副作用重试、有副作用 `needs_recovery`、审批过期。
- 所有 `* -> needs_recovery` 都有同事务 `node.needs_recovery` 事件。
- 启动撤销未过期租约不依赖 TTL，重复扫描幂等。
- `rt_` ID round-trip；四个 `routine.*` 事件 round-trip。
- 健康检查旧字段不消失，新增 live/ready 路由。

## 后果

- 进程重启后，未执行的审批会作废并要求重试；这是有意的安全取舍，UI 必须明确提示“审批已因进程重启失效”。
- `node.needs_recovery` 事件增加一条事件族，但解决了恢复状态不可观测的问题。
- 不实现假续跑，避免在真实副作用上产生重复执行；未来实现成本更高，但契约诚实。
