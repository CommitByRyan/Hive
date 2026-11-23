# Hive 共享白板

> 唯一操作看板。所有对话先读本文件及自己的 runbook。
> 只有 H00 总控可以修改本文件；其他阶段通过 handoff 汇报，不直接改白板。
> 约定：完成的对话**不归档**，保留在侧栏供回看；归档记录当前为空。
> 最近更新：2026-11-23

## 当前门

**H06 — 多 Agent 协作与 worktree（W5）**

目标：消除 AgentRunner 双重租借、实现结构化 Handoff 与独立 worktree 生命周期，让两个 Agent 可并行改不同目录而不互相污染。

状态：`in_progress`

## 阶段状态

| 阶段 | 对话职责 | 状态 | 依赖 |
|---|---|---|---|
| H00 | 总控、契约、集成、发布门 | active | 无 |
| H01 | 契约与工程骨架 | done, verified | H00 |
| H02 | 持久化、事件日志、恢复 | done, verified | H01 |
| H03 | 工具、审批、工作区安全 | done, verified（shell/Win32 留 H03b） | H02 |
| H04 | 单 Agent 垂直闭环 | done, verified | H02、H03、H07 |
| H05 | 并行 DAG 调度（状态机） | done, verified | H04 |
| H05b | 线程化执行器 + 语义收紧 | done, verified | H05 |
| H06 | 多 Agent 协作与 worktree | in_progress | H05b |
| H07 | 模型网关与路由 | done, verified（无真实 HTTP 适配器） | H01 |
| H08 | 服务协议与 Web 控制台 | blocked | H05、H07 |
| H09 | 定时、恢复、可观测性 | blocked | H02、H08 |
| H10 | 评估、故障注入、性能 | blocked | H06 |
| H11 | 发布与迁移 | blocked | H10 |

## 活跃工作

| 任务 | 所有者 | 允许范围 | 基线 | 状态 |
|---|---|---|---|---|
| H01 | H00 | `crates/hive-types`、`crates/hive-config`、`crates/hive-cli`、工程根配置、H01 runbook | 契约修正后工作区 | done |
| H02 | H02 实施对话（pro → 429，切 `auto/deepseek-v4.1-flash` 完成） | `crates/hive-store`、`crates/hive-events`、迁移与测试 | H01 verified 工作区 | done |
| H03 | H03 实施对话（pro 429，改 `auto/deepseek-v4.1-flash`） | `crates/hive-policy`、`crates/hive-tools`、migration V2 | `work/wt-h03`，`codex/h03-tools` | done, merged |
| H07 | H07 实施对话（`auto/deepseek-v4.1-flash`） | 新建 `crates/hive-llm` | `work/wt-h07`，`codex/h07-gateway` | done, merged |
| H03b | 待派发 | 真实 Shell（argv/超时/进程树）、Win32 句柄级路径、连接级 IP 固定 | 新 worktree | pending |
| H04 | H04 实施对话（`auto/deepseek-v4.1-flash`） | `crates/hive-agent`、`hive-llm` ureq transport、migration V3、审批门 | `work/wt-h04`，`codex/h04-agent` | done, merged |
| H05 | H05 实施对话（`auto/deepseek-v4-pro`） | 新建 `crates/hive-runtime`、`CancellationToken` 上移 `hive-types` | `work/wt-h05`，`codex/h05-scheduler` | done, merged |
| H05b | 可见对话 `01a207e0-8bd5-7ff1-8247-78c2156c82f3`（pro） | `ThreadedExecutor`、`AgentRunner: NodeExecutor`、语义收紧 | `.codex/worktrees/36e0/Hive` | done, merged |
| H06 | 可见对话 `01a2080c-0533-7513-81c2-80f64f285384`（pro） | `crates/hive-workspace`、Handoff V4、租约复用 | `.codex/worktrees/3d26/Hive` | implementing |
| H09 设计 | 可见对话 `01a20812-2ebb-7e32-b375-a703c9313edc`（pro high） | 只写 `docs/design/H09-recovery-observability.md` | main `f5ef9b4` | designing |

合并基线：`f5ef9b4`（H05b verified + LF 规范化）。H00 已解决 `Cargo.toml` member 与 `Cargo.lock` 冲突；H05b 合并后全量复验通过。

仓库卫生：`1453238` 因 `core.autocrlf=true` 把全仓重写成 CRLF；`f5ef9b4` 已加 `.gitattributes`（`* text=auto eol=lf`）并规范化，当前相对 `18f8d80` 只剩 3 个文件 7 行差异。

## 冻结契约

- 单进程、多 Tokio task；HTTP/UI 不是核心状态所有者。
- Mission → Run → Node → Agent Session 是统一执行层级。
- DAG 节点必须支持租约、重试、取消、预算和恢复。
- 事件流是客户端增量状态的权威来源；状态转移与事件必须同事务提交。
- 多 Agent 协作以结构化 handoff 为主，不倾倒完整对话历史。
- 代码写入 Agent 默认使用独立 worktree；集成者负责最终合并。
- 外部工具统一经过参数校验、权限策略、审批、执行、登记。
- 未授权的路径、网络、Shell 和子进程访问必须拒绝或进入审批。
- 不把 Python 原型的串行任务模型照搬到 Rust。
- 事件以 `seq` 为唯一游标，`event_id` 只作为稳定身份；重试 attempt 与租约 token 分离。
- Node 必须有重试耗尽终态、恢复态和完整 Run 状态机。
- 单个 Node 审批不得阻塞其他并行分支。

## 模型路由

原则：契约与深水区用推理强的模型，实现用工具调用稳的模型，UI 与协议整合用长文模型；同一对话可在阶段边界换模型，但**不要在一轮实现中途换**。

| 阶段 | 初始模型 | 换模时机 |
|---|---|---|
| H00 总控/契约/集成 | `workbuddy/deepseek-v4-pro` | 需要独立复审时另开 kimi 对话 |
| R0x 独立审计 | `workbuddy/kimi-k3.1-pro` | 只读，不写代码 |
| H01/H02 实现主体 | `workbuddy/deepseek-v4.1-flash` | schema/恢复设计段落切 pro |
| H03 工具与安全 | `workbuddy/deepseek-v4-pro` | 平台细节审查可交 mimo |
| H04/H05 执行内核 | `workbuddy/deepseek-v4-pro` | 样板代码段落可切 flash |
| H06 多 Agent/worktree | `workbuddy/deepseek-v4-pro` | 文档与 runbook 段落可切 flash |
| H07 模型网关 | `workbuddy/deepseek-v4.1-flash` | 流式/重试语义切 pro |
| H08 Web/API | `workbuddy/glm-5.3` | SSE 并发段落切 pro |
| H09 定时/可观测 | `workbuddy/deepseek-v4.1-flash` | — |
| H10 测试/基准 | `workbuddy/deepseek-v4.1-flash` | 故障注入设计切 pro |
| H11 发布/迁移 | `workbuddy/deepseek-v4-pro` | 发布前另开独立审计对话 |

禁止：`mimo-v2.6-pro` 不单独承担大批量 apply_patch（H01b 已出现多次工具调用格式错误），只用于平台/安全审查。

## 执行波次

| 波次 | 对话 | 并行方式 |
|---|---|---|
| W1 | H02 实现 | 单写者 |
| W2 | H03 实现 + H07 实现 | 各自 worktree，无共享文件 |
| W3 | H04 单 Agent 闭环 | 单写者（依赖 H02+H03） |
| W4 | H05 调度 | 单写者 |
| W5 | H06 协作 + H08 控制台 | 双 worktree |
| W6 | H09 + H10 | 双 worktree |
| W7 | H11 + 独立审计 | 审计只读 |

进入 W2 之前必须有一次基线 commit，否则 worktree 不可用，只能顺序执行。

## 阻塞与决策

| 项 | 影响 | 需要谁处理 |
|---|---|---|
| 本机没有 `rustc`、`cargo`、`rustup` | 无法编译、格式检查、单测 | 用户确认安装后由 H00 执行 |
| 尚无 Git 基线提交 | 无法创建稳定 worktree，H02 仍须单写者 | H00/用户决定是否 commit |
| H01 实施对话出现工具调用异常 | 已归档，由 H00 收敛并验证 | 已完成 |
| 误建全局 `C:\Users\24096\.rustup`（约 104 MB，未完成） | 不阻塞，但与项目工具链无关 | 等用户确认是否删除 |
| H02 首轮 26 分钟后遇 magpie 429 | 已换模型续做，不丢已落盘代码 | 已完成切换 |

## 交接收件箱

| 来源 | 交接文件 | 状态 |
|---|---|---|
| R01 | `docs/audits/R01-contract-review.md` | conditional-pass |
| H01 | `docs/runbooks/H01.md` | done, verified |
| R03 | `docs/design/H03-tool-security.md` | design ready（181 行，18 条验收） |
| R07 | `docs/design/H07-model-gateway.md` | recon done（5 次真实探测） |
| R02 | `docs/audits/R02-contract-consistency.md` | fail → 12 项已裁决，见下 |
| H08 | `docs/design/H08-http-console.md` | design ready（186 行，16 条测试断言） |

## R02 裁决（已写入 PROTOCOL）

| 编号 | 裁决 |
|---|---|
| B01 | `AttemptNo` 唯一公共类型，非零 `u32`；事件里 Node 级必填、Mission 级为 `null`；`next` 溢出受检 |
| B02 | `retry_budget` = 首次之后的额外次数，默认 3（最多 4 attempts）；失败时 `attempt_no <= retry_budget` 才 `retry_ready`；递增发生在下次领取 |
| B03 | Mission 删除 `WaitingApproval` 持久态，改为读取层 `MissionObservation` |
| B04 | `seq` 限 `1..=2^53-1`，越界报 `sequence_exhausted` |
| B05 | 快照 = 按 `last_seq` 的物化缓存，非权威、不做事件压缩 |
| B06 | 表归属：V1 H02 / V2 H03 / V3 H04 / V4 H05 / V5 H06 / V6 H09 |
| B07 | `lease_owner` 仅诊断，授权只认 `lease_token`；重启换 instance id |
| B08 | Mission/Run 取消级联未终态 Node 与 pending 审批；提交需 token 与状态双校验 |
| B09 | 预算单位与所有权归 H05，usage 标 estimated/authoritative，超限是失败原因非新状态 |
| B10 | 新增 `node.tool_started`、`node.budget_exceeded`、`model.retry`、`model.fallback` |
| B11 | H02 runbook 改为 `seq` 单调，`event_id` 仅幂等 |
| B12 | 迁移链只追加，V1 冻结，`SCHEMA_VERSION` 唯一来源是 `hive-store` |

## 完成记录

| 时间 | 阶段 | 结果 | 验证 |
|---|---|---|---|
| 2026-11-23 | 初始化 | 创建空 Git 仓库，确认 Rust 工具链缺失 | `git status`、工具链排查 |
| 2026-11-23 | R01 | 契约预审 conditional-pass | `docs/audits/R01-contract-review.md` |
| 2026-11-23 | H00 | 修正事件 ID、状态机、审批、安全与迁移契约 | `docs/PROTOCOL.md`、ADR 0002/0003/0004 |
| 2026-11-23 | H01 | 三个基础 crate、16 个测试、doctor 通过 | fmt、clippy、test、doctor、build |
| 2026-11-23 | H00 | 新增 `docs/ORCHESTRATION.md`、模型路由、执行波次、`scripts/cargo.ps1`、`scripts/verify.ps1` | `cargo --version` 经封装脚本通过 |
| 2026-11-23 | R03 | H03 安全设计：12 类攻击面、18 条验收测试、Windows 句柄级策略 | 只读产出，未改代码 |
| 2026-11-23 | R07 | Magpie 实测：models 扩展字段、stream 末块 usage、tool_calls 字符串参数、400 错误体 | 5 次本机探测，未验证项已标注 |
| 2026-11-23 | R02 | 契约一致性审计 fail，12 项阻塞已全部裁决并写入 PROTOCOL | 审计报告 + 白板裁决表 |
| 2026-11-23 | H02 | V1 schema（含 approvals）、逐步迁移、事务事件、CAS、快照、租约回收、EventBus | 独立复验：fmt/clippy 干净，59 tests 全过 |
| 2026-11-23 | H00 | 新增 `docs/PYTHON_TO_RUST.md`、`scripts/` 封装、基线提交 | `git log`、`scripts/verify.ps1` |
| 2026-11-23 | H03 | `hive-policy`（路径/网络/注册表/hash）+ `hive-tools`（dispatcher、fs.*、shell 拒绝）+ migration V2 + 审批消费 CAS | 分支自验 90 passed / 2 ignored；合并后复验通过 |
| 2026-11-23 | H07 | `hive-llm`：wire 类型、SSE 增量解析、tool_call 累积、模型目录、重试策略、MockTransport | 35 条测试；合并后全量 125 passed |
| 2026-11-23 | H00 | W2 合并：`Cargo.toml`/`Cargo.lock` 冲突已解，main 全绿 | `scripts/verify.ps1`、`cargo test --workspace` |
| 2026-11-23 | H04 | V3 迁移（sessions/turns/usage）、`UreqTransport`、`AgentRunner` 闭环、取消、`StoreApprovalGate` | 独立复验 138 passed / 3 ignored；V3 列已冻结进 PROTOCOL §10 |
| 2026-11-23 | H08 | HTTP/SSE 与控制台设计（186 行，16 条测试断言） | 只读产出，未改代码 |
| 2026-11-23 | H05 | `hive-runtime`：DAG 校验、就绪队列、`max_parallel`、取消、重试、事件序列；`CancellationToken` 上移 | 独立复验 156 passed / 3 ignored |
| 2026-11-23 | H05b | `ThreadedExecutor`（每 worker 独立 Store）、`AgentRunner: NodeExecutor`、attempt 领取时递增、重试同事务、Clock 注入、取消级联 pending | 独立复验 166 passed / 3 ignored |
| 2026-11-23 | H00 | 冻结 `hd_` 前缀、`handoff.created` 事件、V5 handoffs 列契约 | `docs/PROTOCOL.md` |

