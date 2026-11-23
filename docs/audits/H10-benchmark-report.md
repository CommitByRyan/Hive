# H10 评估与故障注入 — 验收报告

> 状态：harness 与故障注入已完成并验证；性能基准数字未采集（交 H00 统一采集）。
> 原则：本对话只保证 harness 正确，不做任何未采集的性能结论。

## 1. 交付范围

新增 `crates/hive-eval`（lib + bin），根 `Cargo.toml` 增加 member，`Cargo.lock` 随构建更新。未改动任何核心 crate 的生产语义。

| 组件 | 位置 | 说明 |
|---|---|---|
| 确定性 fake | `crates/hive-eval/src/fakes.rs` | `ScriptedTransport`、`ScriptedToolExecutor`、`SucceedingExecutor` |
| 场景 harness | `crates/hive-eval/src/scenario.rs` | 6 个场景，稳定 JSONL schema |
| 指标/临时目录 | `crates/hive-eval/src/harness.rs` | `RunRecord`、`TempDir`、commit 采集 |
| CLI | `crates/hive-eval/src/main.rs` | `scenario` 与内部 `kill-worker` |
| 故障注入测试 | `crates/hive-eval/tests/fault_injection.rs` | 8 条 |
| 安全回归 | `crates/hive-eval/tests/security_regression.rs` | 3 条 |
| 场景回归 | `crates/hive-eval/tests/scenarios.rs` | 6 条 |

## 2. 确定性 fake

- `ScriptedTransport`：实现 `hive_llm::ChatTransport`，按请求序号弹出预设 `200/429/500/超时/畸形 JSON`，内部 `Mutex<VecDeque>` 线程安全，可由多个 worker 经 `Arc<Gateway<_>>` 共享；不访问网络。
- `ScriptedToolExecutor`：实现 `NodeExecutor`，按序返回 `Success / PolicyDenied / ExecutionFailure / HangThenCancel` 四类结果，步骤队列经 `Arc` 可跨 worker 共享。
- `SucceedingExecutor`：恒成功，用 `Barrier` 确定性强制 8 个并行节点以 `max_parallel=4` 成波重叠（无 `sleep` 伪造并发），记录峰值并发度。

## 3. 场景集与断言

| 场景 | 断言 | 结果 |
|---|---|---|
| `s1-single` | `node.ready -> node.started -> node.succeeded` 有序，1 条 handoff | ok |
| `s2-dag` | 10 节点全部成功，`node.started` 无重复，峰值并发 ≤ 4 | ok |
| `s3-handoff` | 3 条 `handoff.created` 唯一、`base_revision` 非空、无整段对话倾倒 | ok |
| `s4-retry` | 首 attempt 500 重试、恰好 1 条 `node.failed`、attempt 递增一次后 succeeded | ok |
| `s5-budget` | token 硬上限触发 `node.budget_exceeded`（含 dimension/hard_limit）、`failed_final`、无 `node.ready` 自动重试、下游 skip | ok |
| `s6-cancel` | 运行中取消：未终态节点 `cancelled`、无 `node.tool_started`、重启恢复扫描不重放 | ok |

## 4. 故障注入矩阵（`cargo test -p hive-eval`）

| 故障 | 注入方式 | 断言 | 结果 |
|---|---|---|---|
| 模型 429/5xx | `ScriptedTransport` | 网关有界重试；耗尽后 `failed_final`，`node.failed` 不重复 | ok |
| 模型超时 | `ScriptedTransport::timeout` | 超时可重试；耗尽后返回 `Timeout` | ok |
| 工具策略拒绝 | `ToolDispatcher` + `DenyApprovals` | `node.tool_result` 记录拒绝，不执行副作用 | ok |
| 工具执行失败 | `ScriptedToolExecutor::ExecutionFailure` | 按 retryable 规则重试后 succeeded | ok |
| 数据库锁 | 第二连接 `BEGIN IMMEDIATE` | `busy_timeout` 内成功，事件不丢 | ok |
| 强杀恢复 | `taskkill /F` 杀 `kill-worker` 子进程 | `running` 无终态 → `needs_recovery`，不自动重放 | ok |
| 磁盘慢写 | — | 未实现，仅标注 | 未实现 |

## 5. 安全回归（`cargo test -p hive-eval --test security_regression`）

- `shell.enabled=false` 零进程创建：`shell.run` 返回 `shell.disabled`，无 `node.tool_started`（H03 的 `shell.run` 是提前拒绝的 stub，无 spawn 代码路径）。
- 路径逃逸：`fs.read` `../secret.txt` 在 dispatcher 层返回 `path.outside_workspace`，无 `node.tool_started`。
- 未审批高风险工具：`fs.write` + `DenyApprovals` 返回 `approval.required`，无 `node.tool_started`，不落盘。

## 6. 复现命令

```powershell
$env:HIVE_TOOLCHAIN_ROOT='D:\Downloads\24096\Documents\AAA_codex_code\Hive'
pwsh -NoProfile -File scripts/verify.ps1
pwsh -NoProfile -File scripts/cargo.ps1 test --workspace
pwsh -NoProfile -File scripts/cargo.ps1 run -p hive-eval -- scenario s1-single --runs 1
```

每个场景支持 `--runs <n>` 与 `--out <path.jsonl>`，输出稳定 JSONL；`s5-budget` 已接入并输出 `ok:true`。

## 7. 场景样例（commit `b1a108c`，单次运行）

```jsonl
{"scenario":"s1-single","commit":"b1a108c","run":1,"workers":1,"nodes":1,"wall_ms":46,"events":6,"db_bytes":225280,"ok":true}
{"scenario":"s2-dag","commit":"b1a108c","run":1,"workers":4,"nodes":10,"wall_ms":314,"events":31,"db_bytes":241664,"ok":true}
{"scenario":"s3-handoff","commit":"b1a108c","run":1,"workers":1,"nodes":3,"wall_ms":127,"events":16,"db_bytes":237568,"ok":true}
{"scenario":"s4-retry","commit":"b1a108c","run":1,"workers":1,"nodes":1,"wall_ms":109,"events":10,"db_bytes":233472,"ok":true}
{"scenario":"s5-budget","commit":"b1a108c","run":1,"workers":1,"nodes":2,"wall_ms":78,"events":6,"db_bytes":225280,"ok":true}
{"scenario":"s6-cancel","commit":"b1a108c","run":1,"workers":1,"nodes":3,"wall_ms":40,"events":8,"db_bytes":4096,"ok":true}
```

## 8. 未实现 / 未采集 / 已知限制

- **性能结论**：p50/p95、`events/sec`、线性扩展、内存等未采集；由 H00 在无并行编译的机器状态下统一采集。本报告中的 `wall_ms` 仅为单次 harness 自测，不作性能论断。
- **`s5-budget`**：已接入 H09a 的 V5 预算账本与 `node.budget_exceeded`；通过真实 `ThreadedExecutor` + `WorkerAgentExecutor` 路径验证。
- **磁盘慢写**：未实现。
- **`db_bytes`**：只统计主 `.db` 文件，不含 `-wal`/`-shm`；`s6-cancel` 因连接未 checkpoint 显示偏小，属预期。
- **`handoff.base_revision`**：生产 `AgentRunner::build_handoff` 当前固定为 `"unknown"`；H10 只断言字段非空且 handoff_id 唯一，真实 git 溯源需后续阶段补（不属本阶段回归）。
