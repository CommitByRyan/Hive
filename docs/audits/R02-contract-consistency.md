# R02 — 契约一致性审计

> 类型：只读审计；日期：2026-11-23。
> 范围：冻结契约、ADR 0001–0004、H01–H11 runbook、H03/H07 设计草案、当前 `hive-types` 与根 `Cargo.toml`。
> 基线：工作区文件均未提交，没有可引用的基线 commit；本报告不评价实现测试是否通过。

## 结论

**fail**：当前契约不足以让 H02 与后续 H03/H05/H07 在不各自补规则的情况下实现。
**阻塞项：12 项**；其中 B01–B05、B12 直接卡住正在进行的 H02，B06–B10 会在 W2/W3/W4 造成接口或事件返工。
**H00 最需立刻裁决的 3 条**：B02（Node 重试预算及计数列）、B03（Mission `waiting_approval` 持久态还是派生态）、B05（H02 快照的权威性与恢复语义）。

## 阻塞项

| 编号 | 冲突点 | 文件与行号 | 后果 | 建议裁决 | 影响阶段 |
|---|---|---|---|---|---|
| B01 | `attempt_no` 同时是 `u32` 和 `u64`，且零值规则不闭合 | `crates/hive-types/src/lib.rs:37`、`:232-258` 的 `AttemptNo(u32)`，`:781`、`:805` 的事件字段也是 `u32`；`docs/design/H03-tool-security.md:86-90` 却用 `u64`。`AttemptNo` 从 1 开始，但 `crates/hive-types/src/lib.rs:780-785` 允许反序列化默认 0；`next()` 在 `:256-258` 未处理溢出 | H03 与 H02/H04 的结构体不能直接互转；数据库、事件和审批摘要可能采用不同宽度。Mission 级事件没有 Node attempt，却会用 0 冒充合法计数；递增至溢出会 panic 或绕回 0 | 以 `hive-types::AttemptNo` 为唯一公共类型，固定非零 `u32`；H03 不再声明原始 `u64`。SQLite 存非负 `i64` 并做有检查转换；`next()` 返回 `Option`/`Result`。Mission 级事件的 `attempt_no` 改为 `Option<AttemptNo>`，Node 级必须为 `Some`，禁止 0 哨兵。PROTOCOL/ADR 只定义语义，不再各写整数宽度 | H00、H01 修正、H02、H03、H04、H05 |
| B02 | `DEFAULT_RETRY_BUDGET = 3` 未说明总尝试数，且没有计数/递减的事务规则 | `crates/hive-types/src/lib.rs:15`；`docs/ARCHITECTURE.md:90`、`docs/PROTOCOL.md:70-72`、`:110-114`、`docs/ADR/0002-execution-recovery.md:10-12` 只写“预算 3 次”。相对地，`docs/design/H07-model-gateway.md:262-266` 明确网关 `max_retries=3` 是总尝试 4 次 | 一个实现会给首次 + 3 次重试，另一个会只给总共 3 次；崩溃重放或重复失败事件还可能重复扣减。若把网关重试也扣 Node 预算，同一请求会被算两层 | Node 预算定义为**首次之后的额外执行次数**，默认 3，即最多 4 个 Node attempt；H07 单次模型请求内部重试独立，不扣 Node 预算。`nodes` 持久化 `attempt_no`（从 1）和领取时已解析的 `retry_budget`；不保存可漂移的“剩余次数”，失败数由 `attempt_no - 1` 得出。当前 attempt 失败时仅当 `attempt_no <= retry_budget` 进入 `retry_ready`；下一 attempt 在实际重新领取/开始执行的事务中递增，不在 `node.failed` 时预递增。该状态更新与事件同事务 | H00、H02、H04、H05、H07 |
| B03 | Mission `waiting_approval` 同时被定义为派生态和可直接转移的持久状态 | `docs/PROTOCOL.md:36-47` 把它画进 Mission 状态机，但 `:74` 与 `docs/ADR/0002-execution-recovery.md:14` 明确是派生观测态；`crates/hive-types/src/lib.rs:328-339` 把它收入公共持久状态枚举，`:376-395` 又允许 `running <-> waiting_approval`。H02 要持久化 Mission 基础表（`docs/runbooks/H02.md:7`），H03 则按派生态实现（`docs/design/H03-tool-security.md:108-110`） | H02 很可能把该值写入 `missions.state`，H03/H05 同时按 Node 查询计算；两个来源在并发审批、兄弟节点完成和恢复时必然分叉 | 持久化 Mission 状态机删除 `WaitingApproval`，只保留真实生命周期状态；读取/API 层单独定义 `MissionObservation` 或可选 `display_state`，按“当前 Run 无可运行 Node 且至少一个 pending 审批”计算。禁止为该派生态写 Mission 状态转移事件；Node `waiting_approval` 仍是持久状态 | H00、H01 修正、H02、H03、H05、H08 |
| B04 | `seq` 的 Rust `u64` 范围超过 SQLite 有符号整数和 Web 安全整数范围 | `docs/PROTOCOL.md:27-32`、`docs/ADR/0002-execution-recovery.md:8`、`crates/hive-types/src/lib.rs:792-806` 都是 `u64`；事件 JSON 示例在 `docs/PROTOCOL.md:118-132` 使用 JSON number，H08 又要求浏览器按游标补齐（`docs/runbooks/H08.md:11-13`） | 大于 `i64::MAX` 无法无损写入 SQLite；大于 `2^53-1` 的 JSON number 在 JavaScript 中会丢精度，可能重复或漏补事件。当前没有边界错误和游标解析规则 | 若保留 JSON number，统一限制 `1 <= seq <= 2^53-1`，该范围也安全落入 SQLite `INTEGER`；分配前检查上限，耗尽时明确报 `sequence_exhausted`，禁止 wrapping。Rust/SQLite/API 入参均做有检查转换，负值、0、超上限拒绝。若一定要支持完整 `u64`，则 SSE/HTTP 必须改成十进制字符串，不能维持当前 number 示例 | H00、H02、H08 |
| B05 | 快照只有交付名和表概念，没有用途、读写者、与事件/状态表的关系 | `docs/ARCHITECTURE.md:59` 把快照分给 `hive-store`；`docs/runbooks/H02.md:7` 要求“快照与重启恢复”，但 `docs/PROTOCOL.md:135-143` 只规定事件持久化规则，`docs/ADR/0002-execution-recovery.md:13` 的恢复顺序也不提快照 | H02 可能分别实现事件压缩、状态缓存或恢复权威；H09 的数据保留若清理事件，又可能让已有快照无法验证。相同表名也无法保证可恢复 | 首版快照定义为**按 `last_seq` 标记的物化状态缓存，不是独立权威，也不做事件压缩**。H02 单写者写，H02 查询/恢复读；快照必须含 `schema_version`、作用域 ID、`last_seq` 和状态版本。状态表、事件和快照更新走同一写事务；恢复先读持久状态与事件校验，快照只在 schema/作用域/游标一致时作为加速缓存。schema 迁移、作用域不符、校验失败即弃用重建；首版不得因存在快照而删除其后仍需的事件 | H00、H02、H09、H11 |
| B06 | 已有公共 ID 不等于已有持久化归属；V1 只承诺四类基础表，H03 却假定更多接口已存在 | ID 见 `docs/PROTOCOL.md:9-23` 和 `crates/hive-types/src/lib.rs:220-230`；H02 仅列 Mission/Run/Node/Event（`docs/runbooks/H02.md:7`）。H03 要求产物登记（`docs/runbooks/H03.md:5-7`），并在 `docs/design/H03-tool-security.md:9-12`、`:108-112`、`:132-139` 消费 ToolCall、审批和追加审计；H04 引入 Session/Turn（`docs/runbooks/H04.md:5-13`），H06 引入 Handoff（`docs/runbooks/H06.md:5-15`） | V1 若给这些 ID 建外键会因无表而失败；不建外键又可能被误认为只能靠事件恢复。H03 会自行扩 ToolCall/audit，H04/H06 会重复发明 Session/Turn/Handoff 状态与查询 | 采用下表“最早需要阶段”作为迁移边界；此前事件可携带 ID，但不得声明指向尚不存在表的 FK。需要当前状态、原子 CAS、重启查询或文件元数据的实体必须建表，不能只靠事件流；H07 只返回 wire 数据，不拥有这些领域表 | H00、H02、H03、H04、H06 |
| B07 | `lease_owner` 只有字段名，没有 owner 身份、权限作用或重启失效规则 | `docs/ARCHITECTURE.md:17`、`:41` 与 `docs/ADR/0001-foundation.md:8-15` 限定单进程；`docs/ARCHITECTURE.md:85-86` 引入 `lease_owner`；H05 又要求两个调度器实例竞争同一节点（`docs/runbooks/H05.md:11-13`） | 实现可能把 owner 当进程 ID、Tokio task、Agent Session 或第二个授权 token；崩溃后未到期租约可能一直被旧 owner 占用，或多 task 无法安全接管 | V1 可预留 nullable `lease_owner/lease_expires_at/lease_token`，H05 冻结领取算法。owner 是一次 runtime 调度实例的稳定实例 ID，仅用于诊断和回收，不是授权凭据；授权只认当前 `lease_token`。单进程内多 task 共用 runtime instance ID，每次领取仍生成新 token。重启生成新 instance ID，并在副作用核查流程中撤销旧实例租约；正常运行回收以数据库时钟判断 `lease_expires_at`，不得仅凭 owner 字符串续租 | H02、H05、H09 |
| B08 | Mission 取消级联已写明，但 Run 取消和运行中 Node 的迟到提交规则不闭合 | Mission 级联见 `docs/PROTOCOL.md:73`，审批级联在 `:92` 只列 Mission/Node；H03 却加入 Run（`docs/design/H03-tool-security.md:101-112`）。旧 token 拒绝只在租约过期条款中规定（`docs/PROTOCOL.md:110-114`），H05 又写成“旧 attempt”拒绝（`docs/runbooks/H05.md:13`），H03 假定取消会失效 lease（`docs/design/H03-tool-security.md:169`） | 若取消不改变 token，只校验 token 的迟到 `node.tool_result` 仍会成功落库；若只校验状态，也无法防止旧 worker 在新 attempt 中覆盖。Run 取消是否停 Node/pending 审批会由两个阶段各写一套 | Mission 和 Run 取消都在同一写事务中级联到其所有未终态 Node，并取消关联 pending 审批；Node 取消进入 `cancelled` 且撤销当前 lease。所有事件、工具结果和产物提交使用 `(node_id, attempt_no, lease_token)` 做原子 CAS，同时要求 Node 仍为允许提交的非终态；**token 与状态检查都要，不是二选一**。运行中的 OS 进程/连接另由取消令牌终止，迟到结果即使到达也拒绝提交 | H03、H04、H05、H08 |
| B09 | token/时间/费用预算分散在 Agent、Node、H05、H07，未定义字段、账本和超限状态 | `docs/ARCHITECTURE.md:12`、`:34`、`:63` 提到预算；H05 负责预算（`docs/runbooks/H05.md:5-14`），H07 已设计估算、落账和 `budget_exceeded`（`docs/design/H07-model-gateway.md:331-342`），但 PROTOCOL 的状态机和事件表（`docs/PROTOCOL.md:34-114`、`:145-178`）没有对应规则 | H07 可能直接把预算超限写成节点/事件状态，H05 又实现另一套；取消、断流时 usage 缺失（`docs/design/H07-model-gateway.md:365-369`）会造成重复记账或记 0。费用与时间的单位也无从持久化 | H00 先冻结字段/单位与所有权，H05 实现 Mission/Run/Node 的预算分配和聚合账本，H04 记录每 Turn usage；H07 只提供单次请求估算、权威 usage 和可取消调用，不直接决定 Node/Mission 状态。token 用 `u64`、时间用毫秒整数、费用用带 currency 的最小货币单位整数；usage 必须标 `estimated/authoritative` 并用请求 ID 幂等校准。预算超限是 Node 的结构化失败原因，非新 Node 状态、不可自动重试；Mission/Run 如何汇合由 H05 明确 | H00、H04、H05、H07、H08 |
| B10 | 必需的恢复、网关和预算事件未进入封闭 `EventKind` | 契约清单为 `docs/PROTOCOL.md:145-178`，代码清单位于 `crates/hive-types/src/lib.rs:659-690`。H03 要求实际 `started` 事实（`docs/design/H03-tool-security.md:121`、`:134-136`），H07 要求 `retry_attempt`、fallback 和 `budget_exceeded`（`docs/design/H07-model-gateway.md:268`、`:329`、`:340-342`；`docs/runbooks/H07.md:13`） | `started` 无法与 intent 区分，崩溃恢复无法可靠判断副作用；网关若自建字符串事件会被 `EventKind::parse` 拒绝，若塞进现有 payload 则 H02/H08 各自发明 schema | H00 冻结最小事件映射：新增 `node.tool_started`，`node.tool_call` 仅 intent，`node.tool_result` 承载 completed/failed/cancelled 结果；增加模型 retry/fallback 的具名事件及预算超限的具名节点失败表示，并明确由 runtime 统一落事件、H07 不直接写库。所有 kind 同步进入 `EventKind` 与 payload schema；不得让 runbook 中的裸词成为隐式协议 | H00、H02、H03、H04、H05、H07、H08 |
| B11 | H02 runbook 又把“事件 ID”说成单调递增，与 UUID/seq 分工冲突 | `docs/runbooks/H02.md:11-14` 写“事件 ID 单调递增且可补齐”；`docs/PROTOCOL.md:27-32` 和 `docs/ADR/0002-execution-recovery.md:8` 规定 `event_id=e_<uuid>` 只作身份，`seq` 才是单调游标 | H02 可能按 UUID 排序/补齐，或重新把事件主键改成整数，直接推翻已裁决的事件模型 | H02 冻结点改为“`seq` 单调递增且可补齐；`event_id` 唯一并用于幂等”，不得给 UUID 增加排序语义 | H00、H02、H08 |
| B12 | 数据库迁移只验收空库到当前版，没有 V1→V2、版本常量所有权和回归边界 | 当前唯一通用常量是 `crates/hive-types/src/lib.rs:9-12` 的 `SCHEMA_VERSION=1`；H02 仅要求空库迁移（`docs/runbooks/H02.md:23-30`），H11 只有通用旧配置/回滚验收（`docs/runbooks/H11.md:9-14`）。H03 的审批增量迁移会改变最新版，但 `hive-types` 又受契约所有者约束（`AGENTS.md:40-42`） | H03 加 V2 时可能改 V1 SQL 让空库测试通过，却没有真实 V1 升级验证；也可能自行改公共常量或保留 1，导致 `user_version` 与代码不一致 | 明确 `SCHEMA_VERSION` 是**最新数据库 schema**，与事件/Handoff 版本无关，变更由 H00/契约所有者执行；SQLite `PRAGMA user_version` 与之对应。迁移链只追加、事务执行，V1 文件一旦合入不得修改。H02 保留 V1 建库 fixture/测试；H03 同时验收 fresh→V2 和 fresh V1→V2，并原样运行 H02 的 V1 幂等/恢复测试，不能只重跑最新空库测试 | H00、H02、H03、H11 |

### B06 的实体落库最早阶段

| 实体/表 | 最早必须持久化的阶段 | 裁决：表或事件 |
|---|---|---|
| `agent_sessions` | H04 | 建表：独立上下文有当前生命周期、取消与恢复查询；事件只记录变更。H02 的 `session_id` 暂不设 FK |
| `turns` | H04 | 建表：一次模型请求的取消、usage 校准和重试幂等不能仅靠 `node.turn` 开始事件表达；H07 不提前建领域 Turn 表 |
| `artifacts` | H03 | 建表：`artifact.saved` 只是通知，路径、SHA-256、大小和读取元数据必须可查询；与当前 attempt/lease 原子登记 |
| `handoffs` | H06 | 建表保存结构化 Handoff 正文/索引，并配事件通知；不得把完整 Handoff 只埋进泛化 payload。Handoff 当前无独立 ID，可用 `(node_id, attempt_no, created_at)` 或由 H06 新增契约 ID |
| `tool_calls` | H03 | 建表：请求摘要、审批绑定、执行阶段和一次性消费需要 CAS；`node.tool_call/result` 不能替代当前状态。H03 在 V2 增加，V1 不先建伪 FK |
| `tool_audit` | H03 | 单独追加表：H03 的 hash chain、`prev_hash/entry_hash` 和禁止 UPDATE/DELETE（`docs/design/H03-tool-security.md:132-139`）无法由可普通修改的 events 表代替 |
| `approvals` | H02 | 按已裁决边界由 V1 承载，H03 仅追加 V2；本报告不再改判其归属 |

## 非阻塞但需补文档

| 编号 | 事项 | 文件与行号 | 为何当前不阻塞 | 需固定的边界 |
|---|---|---|---|---|
| N01 | H07 的 `FinishReason` / `Usage` / `ToolCall` 命名 | `docs/design/H07-model-gateway.md:69-123`、`:184-197`；公共代码目前只有 `ToolCallId`，见 `crates/hive-types/src/lib.rs:226` | PROTOCOL 未定义这三个领域类型，当前没有重复定义或 serde 名称冲突 | 将它们置于 `hive_llm::wire`，不要提升到 `hive-types`。wire `ToolCall.id` 是上游不透明字符串，不能直接当 `tc_<uuid>`；H03 的服务端 `ToolCallId` 在 H04 做显式映射。Usage/FinishReason 也不等于预算账本或 Node 失败原因 |
| N02 | H06 Handoff 尚无通知事件 kind | `docs/PROTOCOL.md:180-211` 定义正文；`docs/runbooks/H06.md:5-15` 要求可重建，但 `docs/PROTOCOL.md:145-178` 无 `handoff.*` | H06 尚未开始，可在不修改既有 kind 语义的情况下新增可选事件，不必阻塞 H02 基础表 | H06 建表时同步新增一个 Handoff 登记事件和 payload schema，使 Web 增量客户端无需轮询表；不得复用 `artifact.saved` 冒充 Handoff |

## 已确认一致

- 事件封套的 `agent` 投影规则、`run_id/node_id/session_id` 可空规则在 `docs/PROTOCOL.md:142-143` 与 `crates/hive-types/src/lib.rs:788-810` 一致；本报告不重复审计已裁决项。
- 11 类前缀 ID 在 `docs/PROTOCOL.md:9-23`、`crates/hive-types/src/lib.rs:18-20`、`:220-230` 一一对应；根 workspace 也已声明 H02 两个成员（`Cargo.toml:3-9`），但目录尚未实现。
- Node 的 `failed_final`、`needs_recovery`、终态集合及 Approval 五态/600 秒规则在 `docs/PROTOCOL.md:49-92` 与 `crates/hive-types/src/lib.rs:487-657` 基本一致。
- Node `waiting_approval` 为持久态、审批等待不阻塞兄弟分支，在 `docs/PROTOCOL.md:74-75` 和 `docs/design/H03-tool-security.md:108-112` 一致；B03 只针对 Mission 层。
- `attempt_no` 与 `lease_token` 是两个概念，在 `docs/PROTOCOL.md:70-72`、`docs/ADR/0002-execution-recovery.md:11` 和 `crates/hive-types/src/lib.rs:232-278` 一致；B01/B02 只处理宽度与计数细节。
- H07 的模型请求重试明确让上层 Node 重试接管流中断/已执行工具场景（`docs/design/H07-model-gateway.md:262-268`），分层方向正确；需补的是 B02/B09 的预算账本边界。
- Python 旧库不自动迁移、失败不改源库，在 `docs/PROTOCOL.md:238-243` 与 `docs/ADR/0003-api-migration.md:10-12` 一致；B12 指的是 Rust 自身 V1→V2，不是 Python 导入。

## 验证证据

- 完整读取了任务指定的全部文档、`crates/hive-types/src/lib.rs` 与根 `Cargo.toml`，并用 `rg -n` 对 `attempt_no`、retry/budget、snapshot、lease/cancel、事件 kind、迁移及实体表名交叉定位。
- 只读检查 `git status --short`：当前文件均未跟踪，故没有可填写的基线 commit；`rg --files crates/hive-store crates/hive-events` 报告目录不存在，说明审计针对的是 V1 计划而非已存在迁移。
- 本报告所有阻塞项均给出具体文件与行号；未运行 Cargo、未安装依赖、未修改协议/ADR/runbook/代码。
