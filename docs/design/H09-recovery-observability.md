# H09 — 预算、Routine、恢复与可观测性设计

> 状态：design ready（只读分析，未实现）
> 所有者：H09 设计对话
> 依赖：H02（V1）、H08（HTTP/SSE）、H05/H05b（调度/执行内核）
> 边界：**不修改** V1–V4 已冻结 SQL、`PROTOCOL.md`、`schema.rs`/迁移链、任何已有源码。V5/V6 表结构与本文所有断言均以“提议待实现冻结”呈现；发现协议缺口只列 ADR 候选。

---

## 0. 结论摘要

1. **预算（V5）**：`token / cost / requests / tool_calls` 为硬上限（在“下一个动作之前”检查），`wall_ms` 只告警（可选硬 deadline 单列）。账本分“限额行 + 物化已用计数器”，落点 **Node 级累计（跨 attempt）**，用单写者 `IMMEDIATE` 事务 + `WHERE consumed + delta <= limit` 做 CAS 扣减；事实来源是 `turn_usage`/`tool_calls`，`request_id` 幂等校准避免重试重复计费。
2. **Routine（V6）**：表达式最小集 `every:<N>m|h`、`hourly`、`daily:HH:MM`，**创建期解析拒绝**非法表达式；幂等触发唯一键 `(routine_id, fire_at_ms)` + `next_run_ms` CAS，同一事务建 Mission 并发 `routine.triggered`。
3. **恢复扫描**：顺序 = 撤销全部存活租约 → 副作用分级（safe 自动 `ready`、uncertain 留 `needs_recovery`）→ 审批过期/再驱动 → 防御性 `retry_ready` 归位；每步都按当前状态 CAS，天然幂等可重入。
4. **可观测性**：结构化 JSON Lines 日志（不新增依赖），事件与日志共用 `trace_id`；指标清单为内存注册表（counter/gauge/histogram），标签只取有界枚举；健康分 `liveness/readiness/degraded`；保留策略按“终态 Mission 整段归档”，**绝不重写 `seq`、绝不部分删除活跃 Mission 事件**。
5. **实现切分**：H09a = 预算 + 恢复（切 `workbuddy/deepseek-v4-pro`）；H09b = routine + 可观测（`workbuddy/deepseek-v4.1-flash`）。

---

## 1. 事实基线（只读证据）

- `SCHEMA_VERSION = 3`（`crates/hive-store/src/schema.rs`）；V1 missions/runs/nodes/events/snapshots/approvals、V2 tool_calls/artifacts/tool_audit、V3 agent_sessions/turns/turn_usage 已冻结。V4 handoffs 属 H06，V5 budgets、V6 routines 属本阶段（`PROTOCOL.md §10`）。
- 事件 `seq` 全局单调 `1..=2^53-1`，`event_id` 唯一，`insert_event` 用 `AUTOINCREMENT` + `MAX_SEQ` 守卫（`crates/hive-store/src/store.rs`）。
- Node 状态机与租约/重试语义见 `PROTOCOL.md §2`；`can_retry`、`classify_recovery`（intent-only → SafeToRetry；started/completed → NeedsRecovery）已存在（`crates/hive-store/src/records.rs`）。
- 现有恢复方法：`reclaim_expired_leases`（leased→ready、running→needs_recovery）、`expire_overdue_approvals`（幂等）、`retry_node`/`abort_node`（needs_recovery 人工处置）、`retry_failed_node_and_emit`/`abort_failed_node`（重试路径）（`store.rs`）。
- usage 已按 `source ∈ {estimated, authoritative}` + `request_id` 记入 `turn_usage`（V3），缺失时 `estimated` 且不为 0（`hive-agent/src/runner.rs::build_usage`）。
- 事件封套已含 `trace_id` 字段（`hive-types/src/lib.rs::EventEnvelope`），但当前 `Scheduler::node_event` 与 `AgentRunner::node_event` 均置 `trace_id = None`，需在 H09a 打通。
- 事件已含 `node.budget_exceeded`、`routine.*`、`model.retry/fallback`、`node.tool_started` 等 kind（`EventKind` 枚举），但均未在生产路径发射。
- H08 已定义 `GET/POST /api/routines` 为 H09 前恒空/`not_available_before_h09`，并预留 `GET /api/health` 固定响应形状（`docs/design/H08-http-console.md`）。

---

## 2. A. 预算账本（V5）

### 2.1 目标

把 H05 负责的“预算分配与聚合账本”落到 V5 表，让 Node 在耗尽预算时进入结构化失败（`budget_exceeded`），并让 H08 只读暴露预算摘要，而不新增 Node 状态、不自动重试、不逐行翻译 Python。

### 2.2 预算维度与硬/软语义

| 维度 | 单位/类型 | 语义 | 检查点 |
|---|---|---|---|
| `tokens` | `u64` | **硬上限** | 每次 `gateway.complete` 之前，比较“节点累计已用 + 本次预估值下限”与限额 |
| `cost` | 最小货币单位整数 + `currency` | **硬上限** | 每次模型请求前用预估单价做预检；权威值到达后结算 |
| `requests` | 计数（每次 Turn = 1，网关内部重试不 +1） | **硬上限** | 每次 `start_turn` 之前 |
| `tool_calls` | 计数 | **硬上限** | 每次 `dispatch` 之前 |
| `wall_ms` | 毫秒 | **只告警**（默认无硬 deadline） | 每 Turn 结束与租约续期时；超软限仅记 metric/log |

理由：`token/cost/requests/tool_calls` 都能在“动作发生前”判定，超限直接拒绝且无副作用；`wall_ms` 强停可能打断已 `tool_started` 的副作用，因此默认只告警。若产品需要硬墙钟，作为**可选 `max_wall_ms`** 单独开启，且只在节点尚未出现 `started` 副作用时允许强停（否则退化为告警 + 等人工），这一点列为 ADR 候选。

### 2.3 账本落点与原子扣减

**落点**：计数在 **Node 级、跨 attempt 累计**。理由：`PROTOCOL.md §2` 明言“预算超限是 Node 的结构化失败原因”，且重试产生新 `attempt_no` 不应重置预算；Node 累计耗尽即 `failed_final`。Mission/Run 级的“限额”用于继承与聚合视图，不单独扣减。

**V5 `budgets` 表（提议，待实现冻结）**：

```sql
CREATE TABLE budgets (
    scope_kind   TEXT NOT NULL CHECK (scope_kind IN ('mission','run','node')),
    scope_id     TEXT NOT NULL,
    dimension    TEXT NOT NULL CHECK (dimension IN ('tokens','cost','requests','tool_calls','wall_ms')),
    currency     TEXT,              -- 仅 cost 维度非空，如 'CNY'
    hard_limit   INTEGER,           -- NULL=不限；>=0 为上限
    soft_limit   INTEGER,           -- 告警阈值；NULL=无
    consumed     INTEGER NOT NULL DEFAULT 0,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_id, dimension)
);
```

设计要点：

- **不给 budget 新 ID 前缀**。`ID_PREFIXES` 已冻结且不含 budget，账本行用 `(scope_kind, scope_id, dimension)` 复合主键即可，无需公开身份。
- **限额与已用同表**：限额行是分配；`consumed` 是物化计数器。物化计数用于 O(1) 检查；**事实权威仍在 `turn_usage`（token/cost/requests）与 `tool_calls`（tool_calls 计数）**，物化计数器可由事实表重算（见 2.5）。
- **原子扣减**：单写者 `IMMEDIATE` 事务内执行

```sql
UPDATE budgets SET consumed = consumed + ?1, updated_at_ms = ?2
 WHERE scope_kind='node' AND scope_id=?3 AND dimension='tokens'
   AND (hard_limit IS NULL OR consumed + ?1 <= hard_limit);
```

受影响行数 = 0 即超限。因一个 Node 只有其当前执行者写自己的计数器，不存在跨执行者竞态；CAS 只防“同窗口重复提交”与重启重放。

- **避免重试重复计费**：usage 以 `request_id` 幂等（`INSERT ... ON CONFLICT(request_id) DO UPDATE`），网关内部 429/5xx/断流复用同一 `request_id` 只结算一次；Node 重试产生新的 Turn（`UNIQUE(session_id, turn_index)`）与新的 attempt，属于真实新增消耗，应正常累计。**计数器增量必须与事实行写入同事务**，禁止“先加计数、事实行失败”的半状态。

### 2.4 超限语义

- **事件**：发射 `node.budget_exceeded`（`EventKind::NodeBudgetExceeded`），payload 含 `{dimension, consumed, hard_limit, estimated}`。
- **状态**：Node 直接 `running → failed_final`（不经 `retry_ready`，**不自动重试**），与“超限是失败原因非新状态、不自动重试”一致。实现上把 `ExecOutcome::Failed { reason: "budget_exceeded", retryable: false }` 交给 scheduler，scheduler 识别该 reason 后走 `abort_failed_node`（`failed → failed_final`）并发射 `node.budget_exceeded`（替代普通的 `node.failed`）。
- **级联**：下游沿用现有 `skip_downstream` 逻辑置 `skipped`；其他并行分支不受影响（与单 Node 审批不阻塞其他分支同理）。
- **可重试**：不自动重试；人工可重新创建 Run，或由 H00 裁决是否允许“提高限额后手动 `needs_recovery` 式重试”，默认不允许复活 `failed_final`。

### 2.5 usage 缺失估算与恢复重建

- **估算口径**：权威 usage 缺失（断流、取消、`finish_reason` 异常）时按 prompt/completion 字符数估算，`source=estimated` 且 **≥1、不为 0**；`request_id` 保留为 `None` 或网关可查询的 ID。成本按 `estimated tokens × 单价` 估算并标记 `estimated`。
- **恢复重建**：从**事实表**重建（非从事件流重建——事件流不含逐 Turn token 数）：
  1. `turn_usage` 按 `(node_id, attempt_no)` 聚合 token/cost/requests，`tool_calls` 按 node 计数，`turns/agent_sessions` 时间戳算 wall_ms。
  2. 用聚合结果重写 `budgets.consumed`（整表重算），并交叉核对历史 `node.budget_exceeded` 事件：若某节点历史上已 `budget_exceeded`，**保持其终态，绝不因重算后“未超”而复活**。
  3. 重建幂等：反复执行结果一致；重建后 `consumed` 与事实表一致（`SUM` 校验）。

### 2.6 H08 health/state 只读预算摘要

- 在 `GET /api/health` 与 `GET /api/state` 的 `stats`/mission 详情中**追加可选**字段，不改 H08 已冻结字段：

```json
"budget": {
  "node_id": "n_...",
  "dimension": "tokens",
  "consumed": 1234,
  "hard_limit": 10000,
  "remaining": 8766,
  "estimated": false
}
```

- 只读、不触发扣减；仅暴露渲染所需维度与数值，**不暴露 `currency` 之外的任何成本/单价敏感配置**。是否把该摘要放进 `/api/health` 还是 `/api/state` 列为 ADR 候选（见 §7）。

### 2.7 数据/状态变化、失败模式、测试断言

**数据/状态变化**：新增 `budgets` 表；`turn_usage`/`tool_calls` 写入时同步更新 `budgets.consumed`；超限时 Node `failed_final` + `node.budget_exceeded`。

**失败模式**：
- 计数器与事实行不同事务 → 半状态（已用与事实漂移）。→ 必须同事务。
- 重试重复计费 → `request_id` 非幂等。→ `ON CONFLICT(request_id)`。
- 估算 usage 记为 0 → 预算形同虚设。→ `estimated` 且 ≥1。

**测试断言**（`hive-store` + `hive-runtime`）：
1. 插入两条同 `request_id` 的 usage，`budgets.consumed` 只累计一次。
2. `UPDATE ... consumed+delta <= hard_limit` 在超限时返回 0 行，且行不被修改。
3. Node 达 token 上限后：状态 `failed_final`，事件序列含 `node.budget_exceeded` 且**不含** `node.ready`（不重试）。
4. usage 缺失场景：`source=estimated` 且 `total_tokens >= 1`。
5. 重建函数执行两次结果一致，且 `consumed == SUM(turn_usage)/COUNT(tool_calls)`。
6. 历史 `budget_exceeded` 节点重建后仍为 `failed_final`。
7. `GET /api/state` 的 budget 摘要不含 API Key/base URL/单价。

---

## 3. B. Routine（V6）

### 3.1 目标

支持本地定时触发 Mission：表达式在创建期校验、触发幂等（同一窗口只一个 Mission）、重启不重复/不风暴、不拖垮进程，并与 H08 预留路由对齐。

### 3.2 表达式最小集与创建期拒绝

支持（对齐 Python 原型但收紧）：

- `every:<N>m|h`：`N` 分钟或小时，钳制 `5m ≤ N ≤ 7d`。
- `hourly`：等价 `every:1h`。
- `daily:HH:MM`：`HH∈0..23`、`MM∈0..59`。

**创建期**解析并算首个 `next_run_ms`，非法返回 `400 invalid_schedule`（稳定错误码 + reason）。**绝不把解析推迟到 tick**；tick 只处理“已校验、已算出 next_run”的行。这样即使历史数据混入非法值，tick 也只会标记并停用该行（`routine.error`），不会抛异常拖垮调度循环——调度循环对单行错误必须 `catch` 后继续。

### 3.3 幂等触发：唯一键与事务边界

**V6 `routines` 表（提议，待实现冻结）**：

```sql
CREATE TABLE routines (
    routine_id   TEXT PRIMARY KEY,           -- 新前缀，见 §7 ADR-1
    name         TEXT NOT NULL,
    goal         TEXT NOT NULL,
    schedule     TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    timezone     TEXT NOT NULL DEFAULT 'Asia/Shanghai',
    last_run_ms  INTEGER,
    next_run_ms  INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_routines_due ON routines(enabled, next_run_ms);

CREATE TABLE routine_fires (
    routine_id   TEXT NOT NULL REFERENCES routines(routine_id) ON DELETE CASCADE,
    fire_at_ms   INTEGER NOT NULL,
    mission_id   TEXT,
    state        TEXT NOT NULL CHECK (state IN ('created','failed')),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (routine_id, fire_at_ms)
);
```

**幂等唯一键**：`routine_fires(routine_id, fire_at_ms)` 复合主键；`fire_at_ms` 是**归一化的触发窗口**（`daily` 取当日 `HH:MM` 的整点毫秒，`every` 取 `base + k·interval` 的整数倍，而非“tick 时刻”）。这样同一窗口即使多个 tick 并发重放，也只会命中同一主键。

**事务边界**（单写者 `IMMEDIATE`）：

1. `SELECT` 到期行（`enabled=1 AND next_run_ms <= now_ms`，按 `next_run_ms` 升序）。
2. 对每行：CAS `UPDATE routines SET next_run_ms=<新值>, last_run_ms=now, updated_at_ms=now WHERE routine_id=? AND next_run_ms=<旧值>`；受影响 0 行 → 已被并发 tick 拿走，跳过。
3. 同一事务：`INSERT INTO routine_fires(routine_id, fire_at_ms, state='created', mission_id=?)`（主键冲突即已触发，幂等跳过）+ 建 `Mission(queued, source=routine)` + 发 `routine.triggered`（payload 带 `mission_id`、`fire_at_ms`）。
4. 任一失败 → `routine_fires.state='failed'` + `routine.error`，**不自动重放同一窗口**（避免重启风暴）。

### 3.4 重启补触发/跳过与时区

- **重启策略：跳过错过的窗口，只触发“下一个到期”**（skip-missed）。理由：单用户本地工具，启动补触发可能瞬间创建大量 Mission（风暴），且错过窗口的价值通常已过时。如需“启动补触发最近 N 个窗口”，列为 ADR 候选。
- **时区：固定 `Asia/Shanghai`，不提供运行时配置**。理由：`every/hourly` 与时区无关；仅 `daily:HH:MM` 依赖本地墙上时钟。当前是单用户、单机、Asia/Shanghai 主机；引入可配置时区会增加 DST/迁移面且无当前需求。schema 保留 `timezone` 列便于未来演进，但实现只接受 `Asia/Shanghai`，其他值创建期拒绝。内部时间戳仍统一毫秒（UTC epoch），`daily:HH:MM` 按 Asia/Shanghai 解释后转 epoch 存 `next_run_ms`。

### 3.5 与 H08 `/api/routines` 对齐

- `GET /api/routines` → `200 {"routines":[...]}`，每项 `{routine_id,name,goal,schedule,enabled,timezone,last_run_ms,next_run_ms,created_at_ms}`。
- `POST /api/routines` → `201 {"routine":{...}}`；`schedule` 非法 → `400 invalid_schedule`；超过上限（沿用原型 50 条）→ `400 routine_limit`。
- `POST /api/routines/{id}` → 更新 `enabled`/`schedule`（启用时重算 `next_run_ms`），发 `routine.updated`；`DELETE /api/routines/{id}` → 发 `routine.deleted`。
- H08 的 `not_available_before_h09` 占位在 H09 实现后由 H00 移除（不属 H09 改文件范围）；`PROTOCOL.md §6` 已要求首版保留 `/api/routines` 与 `/api/routines/{id}`，H09 负责补齐子路径语义。

### 3.6 数据/状态变化、失败模式、测试断言

**数据/状态变化**：新增 `routines`/`routine_fires`；到期 tick 建 Mission + `routine.triggered`；非法/触发失败 → `routine.error`（必要时停用）。

**失败模式**：创建期不校验 → tick 拖垮循环；同一窗口双 Mission；重启重复/风暴；`r_` 前缀与 Run 冲突。

**测试断言**（`hive-store` + `hive-runtime` + `hive-server`）：
1. `POST /api/routines` 对 `""`、`every:1m`（<5m）、`every:8d`、`daily:24:00`、`garbage` 均返回 `400 invalid_schedule` 且不写行。
2. 同一 `(routine_id, fire_at_ms)` 触发两次：仅产生一个 Mission、一条 `routine.triggered`。
3. 两个并发 tick 同时到期：CAS 后恰好一个建 Mission（主键唯一）。
4. 重启后（用注入 Clock）错过窗口：不补触发，只在下一窗口触发一次。
5. tick 遇到一条被手工改坏的 schedule：单行 `routine.error` + 停用，调度循环继续处理其余行。
6. `daily:09:00` 在 Asia/Shanghai 下首触发的 `next_run_ms` 换算正确（epoch 断言）。

---

## 4. C. 启动恢复扫描

### 4.1 目标

进程重启后把未终态 Node 安全地归位：可安全重试的自动续跑，副作用不确定的进 `needs_recovery` 等人工，审批挂起的可再驱动，且整个扫描幂等、可重入、绝不重复执行副作用。

### 4.2 扫描顺序与状态转移

启动由新的 `hive-runtime::recovery` 模块执行，**在调度器接受新工作之前**跑完一轮；随后由周期 `tick`（租约回收 + 审批过期 + routine 到期）维持。

```
1. revoke_all_active_leases()          -- 撤销上一实例所有存活租约（不等过期）
2. expire_overdue_approvals(now)       -- 复用现有幂等方法
3. classify_and_recover()              -- 副作用分级 + 自动/人工归位
4. re_drive_waiting_approvals()        -- 审批已决/未决的再驱动
5. normalize_retry_ready()             -- 防御性归位（理论不可见）
```

**1. 撤销全部存活租约（新增启动路径）**：现有 `reclaim_expired_leases` 只回收“已过期”租约，用于周期 tick；启动时必须**无条件撤销**上一实例的 `leased/running` 租约（旧实例已死，TTL 60s 可能未到）。按状态转移：`leased → ready`（未开始，attempt 不变）、`running → needs_recovery`（副作用不确定）。建议把“单节点租约回收状态转移”抽成私有 helper，供 `reclaim_expired_leases` 与 `revoke_all_active_leases` 复用。本步事件：`leased→ready` 发 `node.ready`；`running→needs_recovery` 发 `node.needs_recovery`（reason=`lease_revoked_on_startup`），不发 `node.failed`。

**2. 审批过期**：复用 `expire_overdue_approvals`；对过期审批对应的 `waiting_approval` Node，语义为“工具按拒绝处理”：发 `node.approval_decided(expired)`（若未发）→ Node `waiting_approval → failed`（reason `approval_expired`），随后走正常重试路径（`attempt_no <= retry_budget` 才 `retry_ready`）。

**3. 副作用分级**：对 `running→needs_recovery` 的节点读其 `tool_calls` 副作用，用现有 `classify_recovery`：
- `SafeToRetry`（空 / 仅 `intent`）→ 自动 `needs_recovery → ready`（`retry_node`，attempt+1），发 `node.ready`。
- `NeedsRecovery`（存在 `started`/`completed`）→ 保持 `needs_recovery`，等待人工 `retry_node`/`abort_node`（复用现有方法，分别发 `node.ready`/最终失败）。

**4. 孤儿审批处置（H00 裁决：ADR 0005 D4，fail-closed）**：H09a 不实现“重装 session 继续执行”，因为当前 `AgentRunner` 不持久化完整消息历史，无法保证不重放。启动时对所有 `waiting_approval`：
- 同事务把 pending 或 approved-未消费的审批置为 `expired`（`decided_by=recovery`、`decision_note=owner_lost`），发 `node.approval_decided(expired)`。
- 当前 attempt 无 `started/completed` 副作用：`waiting_approval -> failed(reason=approval_orphaned)`，再按重试预算走 `retry_ready -> ready` 或 `failed_final`；发 `node.failed`。
- 已有 `started/completed` 副作用：`waiting_approval -> needs_recovery`，发 `node.needs_recovery`，等待人工 `retry_node`/`abort_node`。
- 真正的“审批后续跑”留到 H09c/H12，前提是持久化逐 Turn 消息与工具结果并通过重放测试。

**5. `retry_ready` 归位**：`retry_ready` 是 `failed→retry_ready→ready` 同事务的中间态，理论上不会持久化；若扫描发现残留（异常恢复），防御性 `retry_ready → ready` 并记 metric，不发明新语义。

### 4.3 幂等、可重入、不重复副作用

- 每步查询都带 `WHERE state = ?`，状态已推进则 0 行 no-op；重跑整段扫描结果一致。
- 副作用**永不重放**：已 `tool_started` 的节点只进 `needs_recovery`（不自动重试）；自动续跑的节点要么从未 `started`，要么是 `waiting_approval` 的**继续**而非重放。
- 崩溃窗口缩小：`node.tool_started` 在副作用发生**前**同事务落盘；产物临时文件 + 原子重命名；WAL 正常。窗口 = “`tool_started` 已提交但实际 spawn 尚未发生”的毫秒级区间，且该区间内崩溃会被 `classify_recovery` 判为 `NeedsRecovery`（保守，宁可人工）。

### 4.4 复用边界

- 复用：`expire_overdue_approvals`（原样）、`classify_recovery`（原样）、`retry_node`/`abort_node`（原样）、`retry_failed_node_and_emit`/`abort_failed_node`（重试/终态）。
- 新增：`revoke_all_active_leases`（启动）、`re_drive_waiting_approval`（执行器再驱动）、`rebuild_budget_counters`（§2.5）。这些放 `hive-runtime`/`hive-store`，不碰 `reclaim_*` 的周期语义。

### 4.5 失败模式、测试断言

**失败模式**：只回收过期租约导致“快重启 + 未过期旧租约”双执行者；`waiting_approval` 无再驱动导致审批决策后无人续跑；`running` 自动重试导致副作用重放。

**测试断言**（`hive-runtime` + `hive-store`，注入 Clock）：
1. `leased` 节点租约未过期但旧实例已死：启动扫描后 `ready` 且 `attempt_no` 不变、`lease_token` 清空。
2. `running` 节点仅 `intent` 副作用：扫描后自动 `ready`、`attempt_no+1`、发 `node.ready`。
3. `running` 节点含 `started` 副作用：扫描后 `needs_recovery`，不自动重试、不重复发 `node.started`。
4. `waiting_approval` + 审批已过期：审批保持 expired；无 `started` 副作用时 Node `failed(reason=approval_orphaned)` 并按预算决定是否 `retry_ready`。
5. `waiting_approval` + 审批已 approved 未消费且无 `started` 副作用：审批置 `expired(owner_lost)`，Node `failed -> retry_ready`，不执行任何工具。
6. `waiting_approval` + 已有 `started` 副作用：审批置 `expired`，Node `needs_recovery`，不自动重试。
7. 整段扫描连续执行两次：第二次无任何状态变化（幂等）。
8. 恢复期间不重复执行任何已 `tool_started` 的工具（用计数器 Mock 验证调用次数 = 1）。

---

## 5. D. 可观测性

### 5.1 目标

日志、事件、指标共享同一 trace；提供有界标签的指标；分级健康检查；可测试的数据保留，且不破坏 `seq` 游标。

### 5.2 结构化日志与 trace 共享

- **不新增依赖**：实现一个最小内部结构化 logger，向 stderr 写 JSON Lines，固定字段：

```json
{"ts":"...","level":"info","trace_id":"tr_...","span_id":"...","mission_id":"m_...","run_id":"r_...","node_id":"n_...","attempt_no":1,"session_id":"a_...","tool":"fs.read","model":"auto/auto","msg":"...","target":"hive_runtime"}
```

- **trace 共享**：Node 领取时生成 `trace_id = tr_<uuid>`（复用 `TraceId`），每个 Turn 生成 `span_id`、每次工具调用生成子 `span_id`；`EventInput.trace_id` 与日志同源。实现点：`Scheduler::on_started`/`node_event` 与 `AgentRunner::node_event` 开始写入 `trace_id`（当前为 `None`）。
- 若未来要 `tracing`/`tracing-subscriber`/`metrics`，属“新增依赖”，须 H00 审批（ADR 候选）；H09 默认不引入。

### 5.3 指标清单（类型 + 标签基数上限）

内存注册表（`AtomicU64` counter/gauge + 固定桶直方图），经 `GET /api/health` 或未来 `/metrics` 只读暴露。标签**只取有界枚举**，禁止用 `mission_id/node_id/model` 全量值做标签。

| 指标 | 类型 | 标签（基数上限） |
|---|---|---|
| `hive_nodes_by_state` | gauge | `state`（≤12） |
| `hive_nodes_queued` | gauge | 无 |
| `hive_nodes_running` | gauge | 无 |
| `hive_nodes_waiting_approval` | gauge | 无 |
| `hive_retries_total` | counter | `reason_class`（≤16） |
| `hive_budget_exceeded_total` | counter | `dimension`（≤5） |
| `hive_lease_reclaims_total` | counter | `transition`（`ready`/`needs_recovery`，≤2） |
| `hive_sse_lag` | gauge | `connection_class`（≤8） |
| `hive_sse_dropped_total` | counter | `connection_class`（≤8） |
| `hive_event_write_latency_ms` | histogram | 无 |
| `hive_recovery_seconds` | histogram | `phase`（≤6） |
| `hive_mission_duration_seconds` | histogram | `outcome`（≤6） |
| `hive_tool_calls_total` | counter | `tool`（= allowlist 大小，硬上限 64） |
| `hive_approval_pending` | gauge | 无 |

**基数护栏**：任何标签去重值超过上限即归入 `other` 桶并丢弃具体值，防止 `tool/model` 等字符串造成标签爆炸。SSE lag 按连接类别（如 `console`/`agent`）聚合，不按连接 ID。

### 5.4 健康检查分级

| 级别 | 检查内容 | 失败响应 |
|---|---|---|
| `liveness` | 进程活着、SQLite 连接可 `PRAGMA`、Tokio 运行时可用 | `503` |
| `readiness` | `SCHEMA_VERSION` 已迁移、WAL 可写、调度/worker 与恢复扫描已启动、`model_catalog_ready`（对齐 H08） | `503` |
| `degraded` | 任一：`needs_recovery` 非空、窗口内 `budget_exceeded`、SSE lag 超阈、事件写延迟 p95 超阈、审批积压超阈 | `200` 但 `status:"degraded"` + `checks` |

对齐 H08：保留 `/api/health` 已冻结字段，追加可选 `status`、`checks`、`budget`；严格 1:1 响应或独立 `/api/health/{live|ready}` 的取舍列为 ADR 候选。`degraded` 不返回 5xx（仍可服务，仅降级）。

### 5.5 数据保留与归档

原则：`seq` 是全局单调游标（`AUTOINCREMENT`），**保留清理绝不重写 `seq`、绝不部分删除活跃 Mission 的事件**；删除只对**终态 Mission 整段**进行，留下的 `seq` 空洞不影响单调补偿（`after_seq` 只是过滤器）。

| 数据 | 保留策略 |
|---|---|
| `events` | 活跃（非终态）Mission 永不清；终态 Mission 超过 `RETAIN_DAYS`（默认 30）后整体归档到 `data/archive/<mission_id>/events.jsonl`，再删行 |
| `snapshots` | 随所属 Mission 归档/删除（FK `ON DELETE CASCADE` 已存在） |
| `tool_audit` | 随 Mission 整段归档；归档后**审计校验按 Mission 段独立 genesis**（当前 `verify_tool_audit_chain` 是全局单链，见 ADR 候选） |
| `handoffs` | 永不清（业务关键、体量小），仅随 Mission 手动删除 |
| `routines`/`routine_fires` | 不自动清；`routine_fires` 可按 `created_at_ms` 归档 `state='created'` 且超过 90 天的行 |

**游标安全**：清理后发布 `retained_since_seq`（全局最小保留 seq）到 health/state；客户端发现本地 `after_seq < retained_since_seq` 时**强制重新取快照**，不再尝试增量补偿。

**可测试性**：清理只接受 `now_ms` 与阈值，不读真实时钟；阈值边界（恰好到期/未到期）可断言。

### 5.6 失败模式、测试断言

**失败模式**：部分删除活跃 Mission 事件 → 客户端游标断裂；删事件导致 `MAX(seq)` 变小/重写；标签爆炸；`degraded` 误报 5xx。

**测试断言**（`hive-store` + `hive-server`）：
1. 日志行与同节点 `node.started` 事件携带同一 `trace_id`。
2. 指标标签超限后落入 `other`，且具体值不出现在暴露输出。
3. liveness/readiness 在 SQLite 只读/未迁移时分别按预期失败。
4. 保留清理后 `latest_seq()` 不变、`read_events_after(retained_since_seq)` 正常、活跃 Mission 事件一条不少。
5. 客户端 `after_seq < retained_since_seq` 时收到需重取快照的提示（而非静默丢事件）。
6. 终态 Mission 归档后 `tool_audit` 链在 Mission 段内校验通过。
7. 阈值边界：`created_at_ms == threshold` 不被误删（`<` vs `<=` 断言）。

---

## 6. E. 实现切分建议

### H09a — 预算 + 恢复（先做，依赖 H02/H05b）

- 范围：V5 `budgets` 迁移、`turn_usage/tool_calls` 同事务计数器、`request_id` 幂等、`node.budget_exceeded` 发射与 `failed_final` 路径、`revoke_all_active_leases`、副作用分级自动/人工归位、`waiting_approval` 再驱动、`rebuild_budget_counters`。
- 最小验证：
  - `pwsh -File scripts/cargo.ps1 fmt --all --check`
  - `pwsh -File scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings`
  - `pwsh -File scripts/cargo.ps1 test -p hive-store`
  - `pwsh -File scripts/cargo.ps1 test -p hive-runtime`
- 模型：`workbuddy/deepseek-v4-pro`（预算 CAS 与恢复是深水区；样板测试可切 `workbuddy/deepseek-v4.1-flash`）。

### H09b — Routine + 可观测（后做，依赖 H09a 的恢复循环 + H08）

- 范围：V6 `routines`/`routine_fires` 迁移、表达式解析器 + 创建期校验、幂等触发、tick 循环、`/api/routines` 子路径、结构化 logger、指标注册表、健康分级、数据保留/归档。
- 最小验证：
  - `pwsh -File scripts/cargo.ps1 fmt --all --check`
  - `pwsh -File scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings`
  - `pwsh -File scripts/cargo.ps1 test -p hive-store`（V6 迁移 + 保留）
  - `pwsh -File scripts/cargo.ps1 test -p hive-server`（若 H08 已建 crate；否则集成测试放 `hive-runtime`）
  - `pwsh -File scripts/cargo.ps1 test --workspace`
- 模型：`workbuddy/deepseek-v4.1-flash`（routine/表达式/指标为机械样板；幂等触发事务与保留游标段落切 `workbuddy/deepseek-v4-pro`）。

> 说明：H09a 与 H09b 分属 W6 双 worktree（H09 + H10 并行）；H09a 先合入后 H09b 才能复用恢复循环，因此两段**顺序依赖**，不并行写同一 crate。

---

## 7. ADR 候选与 H00 裁决（以 `docs/ADR/0005-budget-routine-recovery.md` 为准）

以下候选已全部裁决：D1–D11 见 ADR 0005；与本节冲突处按 ADR 执行。

以下只在本文档列出，**不自行改契约**：

- **ADR-1（Routine ID 前缀）**：`PROTOCOL.md §1` 已把 `r_` 给了 Run，Routine 需要新前缀（建议 `rt_`）并扩 `ID_PREFIXES`；否则 Routine 无法生成合法公开 ID。
- **ADR-2（启动租约撤销）**：`PROTOCOL.md §2` 说重启“撤销旧实例租约”，但现有只有 `reclaim_expired_leases`（过期回收）。需明确“启动无条件撤销存活租约”作为独立恢复步骤。
- **ADR-3（waiting_approval 再驱动）**：H00 裁定 H09a 不实现 session 续跑；孤儿审批按 fail-closed 处置（见 ADR 0005 D4）。
- **ADR-4（健康端点形状）**：H08 已固定 `/api/health` 响应；H09 的 `status/checks/budget` 是追加可选字段还是新增 `/api/health/{live|ready}`，需 H00 裁决以保持 H08 契约一致。
- **ADR-5（预算墙钟）**：`wall_ms` 默认只告警；是否提供可选硬 `max_wall_ms`（且仅在无 `started` 副作用时可强停）需 H00 确认。
- **ADR-6（tool_audit 全局链 vs 分段链）**：保留清理会打断现有全局 `verify_tool_audit_chain` 单 genesis 链；改为按 Mission 分段 genesis 校验是一次行为变更（不改 V2 冻结列）。
- **ADR-7（时区可配置）**：本文固定 `Asia/Shanghai`；如需运行时配置，需新增配置项与 DST 语义，属 ADR。
- **ADR-8（依赖）**：是否允许引入 `tracing`/`metrics` 等观测 crate；本文默认不引入，用内部最小实现。
- **ADR-9（needs_recovery 事件）**：新增 `node.needs_recovery`，保证恢复状态可观测；已由 ADR 0005 D3 接受。

---

## 8. Handoff

**状态**：设计完成，未实现。H09 仍依赖 H02/H08；本文件不修改冻结协议、schema、迁移链或任何源码。

**改动文件**：仅新建 `docs/design/H09-recovery-observability.md`。

**验证证据**：只读检查了 `WHITEBOARD.md`、`AGENTS.md`、`docs/PROTOCOL.md`（§2/§4/§10 与 R02 裁决）、`docs/ARCHITECTURE.md`、`docs/design/H08-http-console.md`、`docs/runbooks/H09.md`、`docs/reference/HIVE_BUILD_SPEC.md`（§5/§6.9/§11.10）、`crates/hive-store/src/{schema,store,records,lib}.rs`、`crates/hive-runtime/src/{scheduler,threaded}.rs`、`crates/hive-agent/src/runner.rs`、`crates/hive-events/src/lib.rs`、`crates/hive-config/src/lib.rs`、`crates/hive-types/src/lib.rs`。未运行 cargo。

**遗留风险**：H06（Handoff V4）仍在实现中，`SCHEMA_VERSION` 当前为 3，V4 由 H06 冻结，V5/V6 是追加迁移；`ExecOutcome` 的 budget 识别需在实现时定案；`waiting_approval` 再驱动依赖执行器能“无重放继续”，H04/H05b 的 session 装载路径需复核；H08 尚未实现 crate，健康/指标暴露点可能随 `hive-server` 建立而微调。

**建议下一步**：由 H00 裁决 §7 的 8 条 ADR 候选；确认后先派 H09a（预算 + 恢复）worktree，H09b 待 H09a 合入后接续。
