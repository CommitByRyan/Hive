# R01 — H01 契约只读预审

> 类型：只读审计
> 日期：2026-11-23
> 范围：`docs/ARCHITECTURE.md`、`docs/PROTOCOL.md`、`docs/ROADMAP.md`、`docs/runbooks/H01.md`、`docs/reference/HIVE_BUILD_SPEC.md`（第 5–10 章、第 14 章）
> 基线：初始空仓库，`crates/` 尚未创建，Rust 工具链缺失

## 结论

**conditional-pass**

架构方向（单进程、Mission→Run→Node→Agent Session 层级、事件为增量状态权威、worktree 隔离、policy→approval→execute→audit）与 Python 原型的偏差大多是**有意的改进**，方向正确。但冻结候选文本中存在两处协议自相矛盾（事件标识符、Node/Run 状态机不完整）和若干会迫使 H02/H03/H05/H06/H07 返工的缺口。必须先在 H01 冻结前用 ADR 补齐下列 P0/P1 项，才能放行。

---

## P0 — 阻塞冻结：协议自相矛盾或核心语义缺失

### P0-1 事件存在两套互斥的标识方案

- 证据：`docs/PROTOCOL.md:16` 定义 `Event` 为 `e_<uuid>`；但 `docs/PROTOCOL.md:63` 的事件封套用 `"event_id": 42`（`u64`），`docs/PROTOCOL.md:79` 规定客户端用 `event_id` 游标补齐，`docs/PROTOCOL.md:22` 又说“事件序号是单调递增的 `u64`”。
- 影响：H02 建表时“事件主键到底用 UUID 还是自增序号”没有唯一答案；客户端游标（u64）与 `e_<uuid>` 之间无映射规则，会直接导致 schema 返工。
- 建议：明确 `event_id`（u64，单调递增，持久化主键/游标）为权威，`e_<uuid>` 要么删除、要么改定义为“事件引用别名”。写进 ADR 后再冻结。

### P0-2 Node/Run 状态机不完整，无法支撑 H02/H05

- 证据：`docs/PROTOCOL.md:43-48` 的 Node 状态图里 `failed` 只有一条出边 `-> retry_ready (attempt + 1)`；约束 `docs/PROTOCOL.md:53` 只把 `succeeded/cancelled/skipped` 列为终态，**没有“重试耗尽后的节点终态失败”**，也没有 `retry_ready` 回到 `ready/leased` 的回边。`Run` 在 `docs/ARCHITECTURE.md:33` 被定义为核心层级，但协议中没有任何 Run 状态机。
- 影响：H05 的“分支失败”“重试预算耗尽”无法表达；H02 无法为 Run 建状态字段；节点永久失败如何触发 `mission.failed` 没有规则。
- 建议：补全 Node 的 `retry_ready -> ready` 回边、显式 `failed_final`（或等价终态）与重试预算耗尽规则；补 `Run` 状态机（至少 `created -> running -> done/failed/cancelled`）。作为一条 ADR 一次性冻结。

---

## P1 — 需要 ADR，否则 H02/H03/H05/H06/H07 返工

### P1-1 Mission 与 Node 同时存在 `waiting_approval`，在 DAG 下语义冲突

- 证据：`docs/PROTOCOL.md:29-37` 的 Mission 状态机含 `waiting_approval`；`docs/PROTOCOL.md:46` 的 Node 状态机也含 `waiting_approval`。Python 原型中 mission 级 `waiting_approval` 成立的前提是“步骤串行”（`docs/reference/HIVE_BUILD_SPEC.md:519-534`）。
- 影响：DAG 并行时一个节点等待审批不应阻塞其他分支，但 Mission 级 `waiting_approval` 暗示全局暂停。H05/H06 必须澄清“Mission waiting_approval 何时出现”，否则两处实现会打架。
- 建议：ADR 明确 Mission 的 `waiting_approval` 仅表示“存在未决审批”的观测状态，不阻塞其他 Node；或从 Mission 状态机移除该态、只保留 Node 级。

### P1-2 事件族丢失 Python 的降级/追加重规划/定时事件

- 证据：Python 事件族（`docs/reference/HIVE_BUILD_SPEC.md:537-559`）含 `plan.degraded`、`message.user`、`routine.created`、`routine.error`、`agent.thought`；Rust 首版事件族（`docs/PROTOCOL.md:88-107`）只有 `plan.created/plan.revised`，没有 `plan.degraded`、`message.user`、`routine.*`，也没有 `agent.thought` 的对应物。规划降级是 Python 明确行为（`docs/reference/HIVE_BUILD_SPEC.md:721-735`）。
- 影响：“追加要求”是白板冻结能力（`docs/ARCHITECTURE.md:32`“可包含追加要求”），却没有任何用户消息事件；规划降级、定时任务（H09）在事件族里无落点，H02/H04/H09 会各自补字段，破坏“新增事件不改既有语义”。
- 建议：ADR 补齐 `plan.degraded`、`message.user`、`routine.created/routine.error`（或声明 H09 延后）；明确 `agent.thought` 与 `node.turn` 的关系。

### P1-3 恢复语义只有否定句，缺少正向规则

- 证据：`docs/PROTOCOL.md:56` 只说不把未完成 Mission 简单标记为失败；`docs/ARCHITECTURE.md:84-87` 提了 `lease_owner/lease_expires_at/attempt` 和副作用 `intent/started/completed`，但没定义租约过期阈值、`attempt` 是“租约 attempt”还是“重试 attempt”、如何判定“可续跑 vs 必须失败”、`intent/started/completed` 如何在事件/状态中表达。Python 原型的行为是重启即 `failed`（`docs/reference/HIVE_BUILD_SPEC.md:533`、`1560-1568`）。
- 影响：H02 是恢复的落地阶段，冻结契约却把关键判定全部推给 H02，等于没有契约；`attempt` 一词在 `docs/PROTOCOL.md:53`（重试）与 `docs/ARCHITECTURE.md:84`（租约）两个语境混用，H05 的“旧 attempt 迟到结果不覆盖新状态”会因此误实现。
- 建议：ADR 固定租约过期时间、`attempt` 的唯一定义、副作用三态的事件表示、恢复时“续跑/重试/失败”的判定顺序。

### P1-4 审批状态机与超时未冻结

- 证据：Python 审批是 `pending -> approved | rejected`，600 秒超时按拒绝（`docs/reference/HIVE_BUILD_SPEC.md:623`、`976`）；Rust 协议只有 `node.approval_requested/decided` 两个事件（`docs/PROTOCOL.md:97-98`），`p_<uuid>` 标识（`docs/PROTOCOL.md:18`），没有审批状态枚举、超时规则或重复决定语义。
- 影响：H03 明确要验收“审批超时、重启恢复和重复决定”，但契约里没有可依据的状态与超时值，H03 会自行发明。
- 建议：ADR 冻结 Approval 状态（`pending/approved/rejected/expired` 或等价）、默认超时、重复决定返回、重启后 pending 审批的处理。

### P1-5 安全边界存在过度承诺且无机制定义

- 证据：`docs/ARCHITECTURE.md:102` 要求“防止 DNS rebinding 与重定向绕过”，但原型规格明确承认“DNS rebinding 在解析后仍可能发生，0.1.0 没有做连接级 IP 绑定”（`docs/reference/HIVE_BUILD_SPEC.md:839`）。`docs/ARCHITECTURE.md:101` 要求处理“短路径、junction、UNC、驱动器路径”，而原型只是 `resolve()+relative_to` 的应用层校验（`docs/reference/HIVE_BUILD_SPEC.md:818-829`）。
- 影响：这是第 4 类风险点。契约承诺了比原型更强的能力却未说明机制，H03 验收（junction/UNC/大小写/DNS 变化）要么无法达成，要么以“应用层校验”冒充“防止 DNS rebinding”，落入 `docs/ARCHITECTURE.md:104` 自己警告的“未实现却声称安全”。
- 建议：ADR 明确 DNS 防护做到“解析时校验 + 连接级绑定目标 IP + 禁止/限制重定向”，并写明 Windows 短路径/junction/UNC 的规范化实现要求；对暂时做不到的项显式降级为“已知限制”，不写进“必须”清单。

### P1-6 API/SSE 兼容只说了前缀，未定义映射

- 证据：`docs/PROTOCOL.md:146` 只说“首版路径保持 `/api/*`”，没有枚举 Python 的 14 条路由（`docs/reference/HIVE_BUILD_SPEC.md:866-884`）。SSE 外层从 Python 的 `{"mission_id", "event":{id,task_id,agent,kind,content}}`（`docs/reference/HIVE_BUILD_SPEC.md:955`）变成新封套 `{schema_version,event_id,kind,run_id,node_id,session_id,attempt,created_at,trace_id,payload}`（`docs/PROTOCOL.md:61-73`），二者字段无映射表。
- 影响：H08 无法据此保持旧 Web 前端可用；“保持 `/api/*`”会成为空话；`task_id`/`agent`/`content` 到 `node_id`/`session_id`/`payload` 的迁移会让 H08 自由发挥。
- 建议：ADR 枚举首版必须兼容的端点清单，并给出 SSE 外层字段映射（或明确声明旧前端不复用、只保留 URL 形状）。

### P1-7 数据迁移无前提：Python schema 与 Rust 层级无法直接对应

- 证据：Python 是 `missions/tasks/events/artifacts/approvals/routines/messages` 七张表（`docs/reference/HIVE_BUILD_SPEC.md:432-518`），Rust 是 `Mission→Run→Node→Agent Session`（`docs/ARCHITECTURE.md:22-29`）；Python 没有 Run/Node/租约概念，事件表以 `task_id/agent/content` 组织（`docs/reference/HIVE_BUILD_SPEC.md:537-559`）。
- 影响：旧库基本无法无损自动迁移；H11（发布与迁移）没有任何冻结前提，届时会发现旧事件、审批、产物关联全部需要重映射。
- 建议：现在就用 ADR 声明迁移策略：首版不迁移旧库、只保留/重导入 `artifacts` 与 `missions.goal`，或提供一次性导入器。不要把这个决定留给 H11。

---

## P2 — 细节一致性与可观测问题

### P2-1 `project` 实体未定义却被配额/工作区引用

- 证据：`docs/ARCHITECTURE.md:76` 有“项目级配额”，`docs/PROTOCOL.md:155` 有“项目工作区”，但 `docs/PROTOCOL.md:11-19` 的标识符前缀和 `docs/ARCHITECTURE.md:22-29` 的执行层级都没有 `Project` 实体。
- 影响：H03/H05 无法确定“项目”边界（是目录？是 Mission 容器？）。
- 建议：ADR 定义 Project 或把“项目级”改为“Mission 级/工作区级”。

### P2-2 `Turn` 在执行层级无标识符，`trace_id` 不遵守前缀 UUID 规则

- 证据：`docs/ARCHITECTURE.md:26-29` 把 `Turn` 列入层级，但 `docs/PROTOCOL.md:11-19` 没有 `turn` 前缀；`docs/PROTOCOL.md:71` 的 `trace_id` 是 `"trace-..."`，与 `docs/PROTOCOL.md:9`“所有公共标识符使用带前缀的 UUID v4”不一致。
- 影响：H04 给 Turn 建类型时会自行决定 ID；`trace_id` 的生成规则不统一，影响 H09 可观测性串联。
- 建议：ADR 补齐 `Turn` 的 ID 方案，并明确 `trace_id` 是否复用 UUID 或另立规则。

### P2-3 `skipped`/`cancelled` 语义、`plan.revised` 与追加重规划的边界未定义

- 证据：`docs/PROTOCOL.md:47-48` 只有两个词条；`docs/PROTOCOL.md:91` 的 `plan.revised` 与 Python 追加时重复 `plan.created`（`docs/reference/HIVE_BUILD_SPEC.md:540-545`、`723-735`）没有对应说明。
- 影响：H05 无法区分“分支因取消跳过”与“依赖失败跳过”；H04 无法确定追加是 `plan.revised` 还是新的 `plan.created`。
- 建议：ADR 给 `skipped`/`cancelled` 下定义，并说明 `plan.revised` 的触发条件（运行时重规划 vs 追加）。

### P2-4 H01 交付物与验收命令当前不可运行

- 证据：`Cargo.toml` 声明了 `crates/hive-types`、`crates/hive-config`、`crates/hive-cli` 三个成员，但 `crates/` 目录尚不存在；且本机无 Rust 工具链（`docs/runbooks/H01.md` 已注明）。
- 影响：`cargo fmt/clippy/test/run` 四条验收命令现在必然失败；这是实施进度问题而非契约缺陷，但意味着 H01 不能以“implemented, verified”收尾。
- 建议：H01 收尾时补齐三个 crate 的 `Cargo.toml` 与最小源码，并如实标注 `implemented, unverified`（与 `docs/PROTOCOL.md:171` 一致）。

---

## H01 最小实现是否足以支撑后续阶段

- **H02（持久化/事件/恢复）：不足。** H02 要冻结 schema 与事件 payload，但 P0-1（事件双 ID）、P0-2（Run/Node 状态机）、P1-3（恢复规则）都未解决，H02 一开工就会被迫替协议做决定。
- **H03（工具/审批/工作区安全）：不足。** 审批状态与超时（P1-4）、路径/网络安全机制（P1-5）未定义，H03 的验收项无法据契约落地。
- **H07（模型网关）：基本足够。** `docs/ARCHITECTURE.md:57` 已把“模型路由与能力声明”划给 `hive-config`，H01 的“不读密钥的配置加载”与 H07 的“API Key 不出现在日志/事件/配置输出”方向一致；只需在 H01 的 config 类型里预留 `base_url/model/fallback/capabilities` 字段即可，否则 H07 会补字段（可接受，因为小版本新增字段可选）。

## 放行条件（H00 需先落实）

1. 用 ADR 解决 P0-1、P0-2。
2. 对 P1-1 至 P1-7 各产出一条 ADR 或明确写入 `docs/ARCHITECTURE.md`/`docs/PROTOCOL.md`。
3. H01 收尾时如实写 `implemented, unverified`，不因工具链缺失而宣称完成。

未发现需要否决整个架构方向的 `fail` 级问题。
