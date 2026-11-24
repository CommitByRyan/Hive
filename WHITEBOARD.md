# Hive 共享白板

> 唯一操作看板。所有对话先读本文件及自己的 runbook。
> 只有 H00 总控可以修改本文件；其他阶段通过 handoff 汇报，不直接改白板。
> 约定：完成的对话**不归档**，保留在侧栏供回看；归档记录当前为空。
> 最近更新：2026-11-24

## 当前门

**全部阶段完成（0.1.0 本地发布包）**

目标：H01–H11 全部完成并合并。H11 交付 `.env` 配置、`config`/`db` 迁移 dry-run、启动前备份、发布包/安装 helper、干净演练与回滚文档；独立审计 P0/P1 清零。发布包 `dist/hive-0.1.0-windows-x86_64.zip`，SHA256 `9b32c8d2adaa8b44fad3a9d584ec2f2a3ae2cc4162b2558d142df25e60aa8734`；未 push、未建远端、未签名。当前基线 `ac325a2`（主仓 334 passed / 1 ignored）。

状态：`done`

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
| H06 | 多 Agent 协作与 worktree | done, merged, verified | H05b |
| H06b | 调度器租约注入 NodeExecutor | done, merged, verified | H06 |
| H06c | 真实 Agent 工作线程与 ThreadedExecutor 联调 | done, merged, verified（190/3） | H06b |
| H07 | 模型网关与路由 | done, verified（无真实 HTTP 适配器） | H01 |
| H08 | 服务协议与 Web 控制台 | done, merged, verified | H05、H06、H07 |
| H03b-1 | 真实 Shell：argv 直启、进程树、有界输出 | done, merged, verified（207/3） | H03 |
| H03b-2 | Win32 句柄路径与连接级 IP | done, merged, verified（236/1） | H03b-1 |
| H03b-3 | 出网固定与 Shell 退出语义 | done, merged, verified（297/1） | H03b-2 |
| H03b-4 | fs.* 句柄级读写与安全原子替换 | done, merged, verified（304/1） | H03b-3 |
| H09a | 预算账本 + 启动恢复 | done, merged, verified（217/3 + 主仓全绿） | H02、H08、H06c |
| H09b | Routine 定时与 API | done, merged, verified（286/1） | H09a |
| H09c | 可观测与保留 | done, merged, verified（314/1） | H09b |
| H09d | 恢复入口、租约事件与生命周期 tick | done, merged, verified（291/1） | H09b、R08 |
| H06d-1 | worktree 集成器与回收 | done, merged, verified（232/3） | H06 |
| H06d-2 | Handoff 真实基线 | done, merged, verified（264/1） | H09a、H06d-1 |
| R08 | 安全与并发独立审计 | done, merged（no P0 / 4 P1） | H09a、H03b-2 |
| H10 | 评估、故障注入、性能 | done, verified（release 6 场景 × 6 次，36/36 ok） | H06 |
| H11 | 发布与迁移 | done, merged, verified（334/1；独立审计 P0/P1 清零） | H10 |

## 活跃工作

| 任务 | 所有者 | 允许范围 | 基线 | 状态 |
|---|---|---|---|---|
| H01 | H00 | `crates/hive-types`、`crates/hive-config`、`crates/hive-cli`、工程根配置、H01 runbook | 契约修正后工作区 | done |
| H02 | H02 实施对话（pro → 429，切 `auto/deepseek-v4.1-flash` 完成） | `crates/hive-store`、`crates/hive-events`、迁移与测试 | H01 verified 工作区 | done |
| H03 | H03 实施对话（pro 429，改 `auto/deepseek-v4.1-flash`） | `crates/hive-policy`、`crates/hive-tools`、migration V2 | `work/wt-h03`，`codex/h03-tools` | done, merged |
| H07 | H07 实施对话（`auto/deepseek-v4.1-flash`） | 新建 `crates/hive-llm` | `work/wt-h07`，`codex/h07-gateway` | done, merged |
| H03b-1 | 可见对话 `01a20875-6db0-7561-ad50-2053e7832da2`（pro high） | `crates/hive-tools` 的 `shell.run`、`hive-policy` 程序/参数策略、测试 | `.codex/worktrees/ed04/Hive`，基线 `e720968` | done, merged `d116166`, verified 207/3 |
| H09a | 可见对话 `01a20877-15e5-7ad0-a650-eb3df9f9e3b3`（pro high） | V5 budgets、原子扣减、budget_exceeded、启动恢复扫描 | `.codex/worktrees/b2fb/Hive`，基线 `46d381d` | done, merged `5f3769c`, verified 217/3 |
| H09b | 可见对话 `01a2089f-87cc-7281-8cfb-1a9c348b865b`（flash high） | V6 routines、表达式、幂等触发、tick、`/api/routines` | `.codex/worktrees/2b99/Hive`，基线 `5f3769c` | done, merged `5bcee31`, verified 286/1 |
| H09d | 可见对话 `01a208fb-add5-7ce1-8792-dc2079d9bc55`（`auto/deepseek-v4-pro` high） | 启动恢复接线、reclaim 同事务事件、可停止生命周期 tick | `.codex/worktrees/dec6/Hive`，基线 `5bcee31` | done, merged `da60c17`, verified 291/1 |
| H09c | 可见对话 `01a2090a-116c-7091-bde6-9ffc5d43bce8`（pro/glm 429 后切 `auto/deepseek-v4.1-flash` high） | JSONL 日志、trace、指标、健康分级、终态 Mission 归档与分段审计 | `.codex/worktrees/21ad/Hive`，基线 `da60c17` | done, merged `ae93876`, verified 314/1 |
| H11 | 可见对话 `01a20c32-929a-7951-9b5c-ce54e12557ef`（`auto/deepseek-v4.1-flash` high） | H11a `.env`/config migrate/db migrate dry-run/备份/doctor；H11b 打包、安装、干净演练与发布文档；H11c 安装闭环修复 | `.codex/worktrees/7086/Hive`，分支 `codex/h11-release`，基线 `20bd9a5` | done, merged `ac325a2`, verified 334/1 |
| H11 审计 | 可见对话 `01a20c4d-bef8-7140-b53f-3c77b9c22d3d`（`auto/glm-5.3` max） | 只读复核三提交、dist zip、安装/回滚与演练证据 | main 只读 | done，可合并，P0/P1 清零 |
| H03b-4 | 可见对话 `01a2090b-b745-7ab3-9d02-1a58c4bce038`（pro high → 429，切 `auto/kimi-k3.1-pro` high） | `PathHandle` IO/枚举/替换、fs.* 接线、能力声明 | `.codex/worktrees/93a9/Hive`，基线 `0303089` | done, merged `6dd9dbc`, verified 304/1 |
| H03b-2 | 可见对话 `01a2088d-81b1-7200-b025-66eea2c704b3`（mimo high） | `crates/hive-platform`、Win32 句柄路径、连接级 IP 策略 | `.codex/worktrees/2d29/Hive`，基线 `096edb1` | done, merged `8f3e5f6`, verified 236/1 |
| H03b-3 | 可见对话 `01a208f5-bfc6-7771-91e9-0fd085ecdff4`（`workbuddy/deepseek-v4-pro` high） | `hive-llm` pinned egress、`hive-policy` 精确 endpoint、Shell 非零退出 | `.codex/worktrees/6e0f/Hive`，基线 `822cd9c` | done, merged `0303089`, verified 297/1 |
| H10 | 可见对话 `01a20878-248d-73f3-8924-0dc64342445a`（flash high） | `crates/hive-eval`、s5 预算场景、故障注入 | `.codex/worktrees/ca7e/Hive` | done, merged `f9708e3`, 249/3 + s5 |
| H06d-2 | 可见对话 `01a208ad-955d-7d31-a49a-035e146ec718`（pro high） | `AgentSpec.workspace_root`、`NodeSpec` 可选字段、handoff `base_revision` | `.codex/worktrees/c1ee/Hive`，基线 `f9708e3` | done, merged `b7ee6b6`, verified 264/1 |
| H06d-1 | 可见对话 `01a20896-12bf-70c2-af65-97624eb3eb49`（flash high） | `crates/hive-workspace`、merge preview/merge、recycle | `.codex/worktrees/9744/Hive`，基线 `d116166` | done, merged `b1a108c`, verified 232/3 |
| R08 | 可见对话 `01a208a9-6475-7ce1-997c-5b7fedcd7d36`（kimi high） | 只读审计预算/恢复/Shell/平台安全，只写 `docs/audits/R08-security-concurrency.md` | `.codex/worktrees/d707/Hive`，基线 `8f3e5f6` | done, merged `944e961`, no P0 / 4 P1 |
| H04 | H04 实施对话（`auto/deepseek-v4.1-flash`） | `crates/hive-agent`、`hive-llm` ureq transport、migration V3、审批门 | `work/wt-h04`，`codex/h04-agent` | done, merged |
| H05 | H05 实施对话（`auto/deepseek-v4-pro`） | 新建 `crates/hive-runtime`、`CancellationToken` 上移 `hive-types` | `work/wt-h05`，`codex/h05-scheduler` | done, merged |
| H05b | 可见对话 `01a207e0-8bd5-7ff1-8247-78c2156c82f3`（pro） | `ThreadedExecutor`、`AgentRunner: NodeExecutor`、语义收紧 | `.codex/worktrees/36e0/Hive` | done, merged |
| H06 | 可见对话 `01a2080c-0533-7513-81c2-80f64f285384`（pro → flash 收尾） | `crates/hive-workspace`、Handoff V4、租约复用 | `.codex/worktrees/3d26/Hive` | done, merged, verified; 对话已收尾未归档 |
| H06b | 可见对话 `01a2084e-3999-7702-8969-bf62cade3a40`（pro high） | `NodeExecutor` 租约注入、`ThreadedExecutor` 传 guard、失败不二次结算 | `.codex/worktrees/cff0/Hive` | done, merged, verified（187/3） |
| H06c | 可见对话 `01a20868-96e2-7791-9ccd-609a4d37eb7e`（pro high） | 自持有 `WorkerAgentExecutor`、真实 `ThreadedExecutor` 联调 | `.codex/worktrees/3f45/Hive` | done, merged `46d381d`, verified 190/3 |
| H08 | 可见对话 `01a2084e-56e9-75e2-88e3-07327a218ac6`（glm-5.3 max → pro high） | `crates/hive-server`、11 路由、SSE、最小控制台 | `.codex/worktrees/aa44/Hive` | done, merged `e720968` |
| H09 设计 | 可见对话 `01a20812-2ebb-7e32-b375-a703c9313edc`（pro high） | 只写 `docs/design/H09-recovery-observability.md` | main `f5ef9b4` | done, ADR 0005 accepted |

合并基线：`ac325a2`（H11 合并后；主仓独立复验 `All checks passed`，334 passed / 1 ignored）。H08、H06b、H06c、H10、H03b-1、H09a、H03b-2、H06d-2、H09b、H09d、H03b-3、H03b-4、H09c 的测试均作为不退化门槛；V6 已由 H09b 落地并冻结，H11 未改迁移 SQL。

平台安全现状：`hive-llm` 的 ureq 已接 `NetworkPolicy` 单次解析 + 显式 `TcpConnector + RustlsConnector`，并禁用环境代理与重定向；通用网络工具仍关闭。`fs.read/write/list` 已走授权父目录句柄相对 IO，原子替换用 `ReplaceFileW`/`MoveFileEx` 并复核父目录身份，但 `atomic_replace_handle_relative=false`，仍有已记录的路径级残余窗口；非 Windows 回退不是句柄级隔离。Shell 非零退出已改为结构化 `status:"error"` + `exit_code`。

R08 审计结论：无 P0。P1-1/P1-2 已由 H09d 修复（启动恢复接线、reclaim 同事务事件）；P1-3/P1-4 已由 H03b-3 修复（pinned egress、Shell 非零退出）；P2-2 已由 H03b-4 关闭（fs.* 句柄级，残余窗口已记录）。H09c 额外暴露的保留风险：`retained_since_seq` 仅存在进程内，重启后回到 0；归档尚未接入 tick，需要调用方触发；快照随终态 Mission 删除且未写入归档。P2-1（预算预估下限）、P2-4（approval_expired 与 owner_lost 合并）与 P3 项仍待排期。P2-3 已被 H10 s5 覆盖。

仓库卫生：`1453238` 因 `core.autocrlf=true` 把全仓重写成 CRLF；`f5ef9b4` 已加 `.gitattributes`（`* text=auto eol=lf`）并规范化，当前相对 `18f8d80` 只剩 3 个文件 7 行差异。

契约同步：`PROTOCOL.md` 已加入 ADR 0005 与 ADR 0006；`routine.*` 非触发事件使用系统作用域 `m_00000000-0000-4000-8000-000000000000`，合法但不是 `missions` 行。H09b 已按 ADR 0006 收口并合并：`SYSTEM_SCOPE_MISSION_ID` 为单一常量，`routine.triggered` 使用真实 Mission，`list_missions` 不含系统作用域。

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
| H09c/H03b-4 首轮同时遇 magpie 429 | 两对话均未落代码，worktree 干净 | 已分别切 `auto/glm-5.3` max 与 `auto/kimi-k3.1-pro` high 续跑 |

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
| 2026-11-23 | H06b | 调度器租约注入 `NodeExecutor`；执行器已置 failed 后不再二次结算 | 分支自验 187 passed / 3 ignored；合并后独立复验 `All checks passed` |
| 2026-11-23 | H08 | `hive-server` 11 端点、SSE 持久补偿与 lag、审批、最小控制台 | 合并后独立复验 `All checks passed`；`api.rs` 8、`sse.rs` 2 |
| 2026-11-23 | H06c | `WorkerAgentExecutor<T>` 自持 Store、工厂 helper、真实 `ThreadedExecutor` 三条集成测试 | 分支与合并后均 `All checks passed`，190 passed / 3 ignored（+3） |
| 2026-11-23 | H10 | `hive-eval` 确定性 fake、5 场景、故障注入与安全回归；s5 待 H09a | 分支与合并后均 `All checks passed`，217 passed / 3 ignored |
| 2026-11-23 | H03b-1 | `ShellPolicy`、argv 直启、Job Object/进程组、有界输出、assign 失败清理 | 分支 207 passed / 3 ignored；合并后 `All checks passed` |
| 2026-11-23 | H09a | V5 budgets、原子扣减、`node.budget_exceeded`、启动恢复五步、孤儿审批 fail-closed、AgentRunner 双路径接线 | 分支 217 passed / 3 ignored；合并后 `All checks passed`，迁移 fresh/V1–V4→V5 全绿 |
| 2026-11-23 | H06d-1 | `head_revision`/`status`/`preview_merge`/`merge`/`recycle`，5 条真实 Git 仓库测试 | 分支与合并后均 `All checks passed`，hive-workspace 10 passed |
| 2026-11-23 | H03b-2 | 新建 `hive-platform`：Win32 句柄遍历、junction 拒绝、`PinnedEndpoint`/`resolve_and_validate`/`connect_validated` | 分支 236 passed / 1 ignored；合并后 `All checks passed`；网络未接 ureq，已声明 |
| 2026-11-23 | H10 s5 | `hive-eval` s5-budget：token 硬上限→`node.budget_exceeded`+`failed_final`+下游 skip | 分支 249 passed / 3 ignored；合并后 `All checks passed`；`s5-budget` ok:true |
| 2026-11-23 | H06d-2 | `AgentSpec.workspace_root`、`NodeSpec` 可选字段、handoff 真实 `base_revision`，失败 fail-open 记风险 | rebase 后分支与主仓均 `All checks passed`，264 passed / 1 ignored；新增 6 条测试 |
| 2026-11-23 | R08 | 安全与并发只读审计：no P0、4 P1、4 P2、5 P3 | 审计档 `docs/audits/R08-security-concurrency.md` 已并入 `944e961` |
| 2026-11-23 | H09b | V6 `routines`/`routine_fires`、`rt_` ID、表达式与时区、幂等 fire、tick、`/api/routines`、ADR 0006 系统作用域收口 | 分支与主仓均 `All checks passed`，286 passed / 1 ignored；迁移 fresh/V1–V5→V6 |
| 2026-11-23 | H09d | `run_startup`（恢复→预算重建）、`tick_once`（routine→lease 回收）、server 启动门与可停止 tick、reclaim 同事务事件 | 分支与主仓均 `All checks passed`，291 passed / 1 ignored；新增 5 条测试 |
| 2026-11-23 | H03b-3 | ureq 自定义 `PinnedResolver` + 显式 connector、精确 endpoint、禁代理/重定向、Shell 非零退出结构化 | 分支 270/1；主仓合并后 `All checks passed`，297 passed / 1 ignored |
| 2026-11-24 | H03b-4 | `hive-platform::PathHandle` 句柄持有/相对读写/目录枚举/原子替换，`fs.read/list/write` 接线，junction 父目录拒绝，能力声明 | 分支 304 passed / 1 ignored（NTFS junction 用例真跑）；主仓合并后 `All checks passed`；残余替换窗口已记录 |
| 2026-11-24 | H09c | JSONL 日志与 trace、Atomic 指标注册表、health 分级、终态 Mission 整段归档、Mission 段审计链校验、`retained_since_seq` 快照提示 | 分支 301 passed / 1 ignored；主仓合并后 `All checks passed`，314 passed / 1 ignored；归档 fsync 后同事务删除，`latest_seq` 不降 |
| 2026-11-24 | H10 基准 | H00 在空闲机器用 release harness 采集 6 场景 ×（1 预热 + 5 样本），写入 p50/p95、events/sec、db_bytes 与机器元数据 | 36/36 `ok:true`；报告 `docs/audits/H10-benchmark-report.md` §9，原始 JSONL `docs/audits/data/` |
| 2026-11-24 | H11 | `.env` 解析与严格未知键、`config migrate`/`db migrate --dry-run`（VACUUM INTO 副本）、启动前备份、安装包 doctor 语义、打包/安装脚本、RELEASE/CHANGELOG、18765 干净演练 | `3e08bf5` + `0a3d9c8` + `66d1d5c`；主仓合并后 `All checks passed`，334 passed / 1 ignored；zip SHA256 `9b32c8d2…aa8734` |
| 2026-11-24 | H11 审计 | 独立只读审计三提交、包体 8 项、安装/回滚/迁移证据，复核修复轮 | 结论可合并；P0/P1 清零；P2 迁移失败副本测试已补；P3 无 |

