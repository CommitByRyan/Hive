# R08 安全与并发独立审计

> 范围：预算、恢复、Shell、平台/策略接线、并发与取消、H10 评估。
> 基线：`8f3e5f6`（H03b-2 合并后）；只读审计，不实现功能、不改代码。
> 结论：**未发现可证明的 P0**（数据丢失/重复副作用/安全绕过）。发现 4 个 P1（恢复未接线、租约回收不写事件、出网策略未接线、Shell 非零退出折叠为成功）、4 个 P2、5 个 P3。核心风险是「能力声明/文档高于实际接线」。

## 已验证为正确的关键不变式

- 恢复五步顺序（revoke leases → expire approvals → classify side effects → orphan fail-closed → normalize retry_ready）与 CAS 幂等成立；`recovery.rs:28` 的 8 条测试覆盖 `running`（intent-only → 自动 retry）、`running`（started → needs_recovery）、`waiting_approval` 三分支与「不重放 started 副作用」，全部通过。
- 预算扣减与事实行同事务：`append_tool_event`（store.rs:1399）与 `record_turn_usage`（store.rs:1182）都在单事务内「扣减 + 写事实行」，超限整体回滚，不存在「已扣费但节点未失败」的半状态。
- `fail_budget`（runner.rs:647）走 `transition_node_cas` 同事务发 `node.budget_exceeded`；scheduler `handle_failure`（scheduler.rs:481-496）对 `budget_exceeded` 不重试、不发 `node.failed`、不 emit `node.ready`（测试 `budget_exceeded_finalizes_without_retry_or_node_ready` 确认）。
- `request_id` 幂等：单 turn 内重试复用同一 `request_id` 只结算一次（`budget_request_id_is_idempotent` 通过）。
- Shell：argv 直启（不经 shell 解释）、`env_clear`、Job Object `KILL_ON_JOB_CLOSE`、assign 失败清理孤儿进程、有界输出/截断、超时杀进程树，均有测试并通过（`shell.rs` 3 + `dispatcher` 11）。
- 并发真实而非 sleep：`SucceedingExecutor`（fakes.rs）用 `AtomicUsize` 活跃计数 + `Barrier` 强制 mid 节点重叠，断言 `max_active <= workers`（`run_dag` 用 4 worker / 8 mid / `Barrier::new(4)`）。

## P1

### P1-1 启动恢复未接入任何生产入口
- 位置：`crates/hive-runtime/src/recovery.rs:28`、`crates/hive-store/src/store.rs:1833`、`crates/hive-runtime/src/lib.rs:22`
- 触发条件：进程重启后打开既有数据库。
- 现状：`run_startup_recovery` 与 `rebuild_budget_counters` 仅被 `lib.rs` 导出与单元测试引用。`hive-cli/src/main.rs` 只有 doctor/version；`hive-server/src/main.rs` 打开 Store 后直接 `axum::serve`；`Scheduler::new`（scheduler.rs:239）与 `ThreadedExecutor::run`（threaded.rs:54）都不调用。无周期 tick 循环。
- 影响：ADR 0005 D2/D3/D4 的「启动无条件撤销存活租约、孤儿审批 fail-closed、重建预算计数器」在真实重启路径全部不生效；崩溃遗留的 `leased/running/waiting_approval` 节点不会被归位，可能永久卡住。
- 建议：在 `hive-server::main`（或未来统一 runtime 启动器）先跑 `run_startup_recovery` + `rebuild_budget_counters` 再接受新工作；补一条「重启后旧节点归位」的集成测试。
- 验证：`rg` 全仓（排除测试）仅见导出与测试调用，无生产调用点。

### P1-2 `reclaim_expired_leases` 状态转移不写事件
- 位置：`crates/hive-store/src/store.rs:886`
- 触发条件：周期 tick 或手动调用回收过期租约。
- 现状：`leased -> ready`、`running -> needs_recovery` 直接 `UPDATE nodes`，不发 `node.ready`/`node.needs_recovery`。对比 `revoke_all_active_leases`（store.rs:1547）正确地在同事务写事件。
- 影响：违反冻结契约「状态转移与事件必须同事务提交」与 ADR 0005 D3「所有 `* -> needs_recovery` 必须同事务发 `node.needs_recovery`」；SSE 客户端看不到恢复转移。
- 建议：把单节点租约回收抽成共享 helper，两条路径（启动撤销/过期回收）都复用并在同事务 `insert_event`。
- 验证：读代码确认该函数无 `insert_event`；`revoke_all_active_leases` 有。

### P1-3 出网策略（连接级 IP）未接入 hive-llm
- 位置：`crates/hive-policy/src/network.rs:191`、`crates/hive-llm/src/ureq_transport.rs:51-56`、`crates/hive-platform/src/net.rs:25`
- 触发条件：模型网关发出任何真实 HTTP 请求。
- 现状：`NetworkPolicy::connect_validated`/`connect_pinned` 只被测试调用；`UreqTransport::post` 用裸 `ureq::Agent` 自行解析连接，无 IP 分类/校验/钉扎。`ToolDispatcher` 持有的 `network` 字段（dispatcher.rs:69）在 `dispatch` 中从未被读取。`net_capabilities()` 自报 `dns_rebinding_resistant: false`、note 明说「ureq is not rewired」。
- 影响：H03b-2 的「连接级 IP」是策略层能力，未接到真实模型出网；这是把策略层冒充平台级安全的误导，DNS 重绑定/SSRF 防护名存实亡。当前无模型可调的 network 工具，实际可利用性有限，但能力声明高于实际接线。
- 建议：要么让 `UreqTransport` 走 pinned socket（`connect_validated` → `PinnedSocket.into_stream()`），要么在白板/`H03-tool-security` 明确标注「未接线」，避免误判安全边界。
- 验证：`rg` 无生产调用点；`net.rs:25` 与测试 `network.rs:428` 均承认 `dns_rebinding_resistant=false`。

### P1-4 Shell 非零退出码被折叠为成功
- 位置：`crates/hive-tools/src/shell.rs:134`、`crates/hive-tools/src/dispatcher.rs`（`audit_success` 分支）
- 触发条件：allow-list 内程序退出码非 0。
- 现状：`run`/`run_with` 无论退出码都返回 `Ok(ToolOutput)`，仅把 `exit_code=N` 写进 summary；`dispatch` 对 `Ok` 一律发 `node.tool_result {status:"ok"}`，`append_tool_event` 把 `status=="ok"` 记为 `completed`。
- 影响：失败的 shell 命令被审计为 completed、节点可能被误判成功（错误终态）。helper 有 `exit` 模式但无对应测试。
- 建议：非零退出返回携带 `exit_code` 的 `ToolError`（或至少 `status:"error"`），并补测试。
- 验证：`crates/hive-tools/src/bin/hive_shell_helper.rs` 有 `exit` 分支，`tests/shell.rs` 无对应断言；读代码确认 `Ok` 路径。

## P2

### P2-1 预算 token 预检只查「已耗尽」，不查「预估下限」
- 位置：`crates/hive-agent/src/runner.rs:249`
- 现状：预检只判 `budget_remaining <= 0`，未按设计 §2.2 用「累计已用 + 本次预估下限」在发请求前拦截。超限请求仍被发出、网关计费后才被 `record_turn_usage` 的 CAS 拒回并 fail 节点。
- 影响：预算非预防性，单请求最多可超 `max_tokens`，产生真实成本超支（仍会 fail-closed，不丢数据、不重试）。
- 建议：预检改用估算下限（如 `spec.max_tokens` 或历史均值）与限额比较。

### P2-2 `authorize_handle`（Win32 句柄路径）未接入 fs 工具
- 位置：`crates/hive-policy/src/path.rs:194`、`crates/hive-tools/src/dispatcher.rs:180-183`
- 现状：`fs.read/write/list` 走 `paths.authorize`（字符串 canonicalize + 前缀比较，文档自述存在 TOCTOU 窗口）；`authorize_handle`/`PathHandle` 仅测试引用。
- 影响：H03b-2 的句柄级防逃逸是死代码，平台级安全声明高于实际接线（同 P1-3 主题）。
- 建议：fs 工具切换 `authorize_handle`，或在白板标注「句柄防御未接入生产」。

### P2-3 H10 s5 预算场景仍标记 `pending H09a`
- 位置：`crates/hive-eval/src/scenario.rs:321`
- 现状：H09a 已合并（`5f3769c`），但 `run_budget` 仍返回 `ok:false, blocked:"pending H09a"`，预算路径无端到端评估覆盖。
- 建议：H10 接 s5，用真实 `set_budget` + `ScriptedTransport` 验证 `node.budget_exceeded`。

### P2-4 恢复步骤 2 的 `approval_expired` 与步骤 4 的 `approval_orphaned` 被合并
- 位置：`crates/hive-runtime/src/recovery.rs:39-91`
- 现状：`expire_overdue_approvals`（步骤 2）只过期审批记录；所有 `waiting_approval` 节点统一按 `owner_lost` fail-closed（步骤 4）。设计 §4.2 步 2 的独立 `approval_expired` 分支未实现。
- 影响：fail-closed 仍安全，但事件 `reason` 与设计不符（统一 owner_lost）。待验证：是否需要在后续阶段区分二者。

## P3

- `record_turn_usage` 幂等用 `SELECT EXISTS`（store.rs:1212）而非 `ON CONFLICT(request_id)`；`turn_usage` 用裸 `INSERT`（无 ON CONFLICT）。同一 `request_id` 跨 turn 会跳过扣减，但 `rebuild_budget_counters` 会 `SUM` 两行，live 计数与 rebuild 漂移。当前 runner 每 turn 只记一次，未触发。
- cost 维度按 1 token=1 货币单位占位（store.rs:1230），`rebuild_budget_counters` 对 cost 也 `SUM(total_tokens)`（store.rs:1845），`currency` 列未参与。有文档标注，接真实单价时会出错。
- `set_budget`（store.rs:1315）`ON CONFLICT` 不重置 `consumed`；把 `hard_limit` 调到低于已 `consumed` 会留下负 remaining，下次扣减才 fail（fail-closed 可接受）。
- `shell.run_with` 超时分支若 `TerminateJobObject` 失败仍会 `child.wait()`，存在阻塞风险（shell.rs 超时段）。罕见，待验证。
- `Scheduler::reclaim_expired`（scheduler.rs:295）/`reclaim_expired_leases` 无周期 tick 调用方（仅测试与 eval `run_cancel`）。

## 验证证据

- 命令：`$env:HIVE_TOOLCHAIN_ROOT='D:\Downloads\24096\Documents\AAA_codex_code\Hive'; pwsh -NoProfile -File scripts/cargo.ps1 test -p hive-store -p hive-runtime -p hive-policy -p hive-platform -p hive-tools -p hive-agent -p hive-eval`
- 结果：全部通过，`0 failed`；覆盖 store 47、runtime、policy 32、agent 26、tools 11+3（含 shell 集成）、platform/policy 的 Windows 路径与 TOCTOU/junction 用例（受 NTFS junction 能力影响部分 SKIP 属预期）。
- 只读检查：`git log --oneline -15` 确认基线 `8f3e5f6`；`git status --short` 干净（提交前仅新增本报告）。

## 建议下一步

1. 优先修 P1-1/P1-2：把启动恢复与租约回收事件接入统一 runtime 启动器，并补重启集成测试（属 H09b/H09c 或 H00 集成门）。
2. 明确接线契约：P1-3/P2-2 要么把平台级防御接进生产路径，要么在白板与安全设计文档降级标注「未接线」，杜绝误导。
3. 补 Shell 非零退出语义（P1-4）与 H10 s5（P2-3）。
4. P3 幂等/成本单位问题在接真实单价与多 turn 重试前固化。
