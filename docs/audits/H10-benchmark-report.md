# H10 评估与故障注入 — 验收报告

> 状态：harness 与故障注入已完成并验证；正式性能基准已于 2026-11-24 在空闲机器上采集（见 §9）。
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

## 8. 未实现 / 已知限制

- **性能结论**：已采集，见 §9；§7 的 `b1a108c` 单次样例只作 harness 自测，不再作为性能依据。
- **`s5-budget`**：已接入 H09a 的 V5 预算账本与 `node.budget_exceeded`；通过真实 `ThreadedExecutor` + `WorkerAgentExecutor` 路径验证。
- **磁盘慢写**：未实现。
- **`db_bytes`**：只统计主 `.db` 文件，不含 `-wal`/`-shm`；`s6-cancel` 因连接未 checkpoint 显示偏小，属预期。
- **`handoff.base_revision`**：生产 `AgentRunner::build_handoff` 当前固定为 `"unknown"`；H10 只断言字段非空且 handoff_id 唯一，真实 git 溯源需后续阶段补（不属本阶段回归）。
- **节点级分位**：harness 未暴露单节点完成时间，§9 报告的是场景 `wall_ms` 的 p50/p95；节点级 p50/p95、峰值并行度实时采样、取消延迟细分列为未采集项，不用其他数字替代。

## 9. 正式基准（release，2026-11-24）

### 9.1 环境与方法

| 项 | 值 |
|---|---|
| 采集时间 | 2026-11-24 14:12（Asia/Shanghai） |
| commit | `6eb85f4` |
| 构建 | `cargo build --release -p hive-eval`（优化构建） |
| 二进制 | `target/release/hive-eval.exe`，10,278,666 bytes |
| CPU | 13th Gen Intel Core i9-13900H，14 核 / 20 逻辑线程 |
| 内存 | 31.7 GB |
| 磁盘 | UMIS RPEYJ1T24MKN2QWY，954 GB（NVMe SSD；Windows 报告 `Fixed hard disk media`） |
| OS | Windows 11，10.0.22631 |
| Shell | PowerShell 7.6.10 |
| 工具链 | rustc 1.100.0（0ef1bc386 2026-11-09），host `x86_64-pc-windows-gnu`；cargo 1.100.0 |
| 采样 | 每个场景 1 次预热 + 5 次正式样本（`--runs 6`，统计 run 2–6） |
| 并发控制 | 采集期间无并行编译、无其他基准任务；场景串行执行 |

采集命令：

```powershell
$env:HIVE_TOOLCHAIN_ROOT='D:\Downloads\24096\Documents\AAA_codex_code\Hive'
pwsh -NoProfile -File scripts/cargo.ps1 build --release -p hive-eval
.\target\release\hive-eval.exe scenario s1-single --runs 6 --out docs\audits\data\s1-single.jsonl
.\target\release\hive-eval.exe scenario s2-dag    --runs 6 --out docs\audits\data\s2-dag.jsonl
.\target\release\hive-eval.exe scenario s3-handoff --runs 6 --out docs\audits\data\s3-handoff.jsonl
.\target\release\hive-eval.exe scenario s4-retry  --runs 6 --out docs\audits\data\s4-retry.jsonl
.\target\release\hive-eval.exe scenario s5-budget --runs 6 --out docs\audits\data\s5-budget.jsonl
.\target\release\hive-eval.exe scenario s6-cancel --runs 6 --out docs\audits\data\s6-cancel.jsonl
```

原始 JSONL 已随仓库提交：`docs/audits/data/<scenario>.jsonl`。

### 9.2 结果（5 次正式样本）

| 场景 | workers | nodes | wall p50 (ms) | wall p95 (ms) | events p50 | events/sec p50 | db_bytes p50 | 全部 ok |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| `s1-single` | 1 | 1 | 110 | 158 | 6 | 54.5 | 245,760 | 是 |
| `s2-dag` | 4 | 10 | 223 | 343 | 31 | 139.0 | 262,144 | 是 |
| `s3-handoff` | 1 | 3 | 178 | 342 | 16 | 89.9 | 258,048 | 是 |
| `s4-retry` | 1 | 1 | 156 | 201 | 10 | 64.1 | 253,952 | 是 |
| `s5-budget` | 1 | 2 | 117 | 161 | 6 | 51.3 | 245,760 | 是 |
| `s6-cancel` | 1 | 3 | 53 | 67 | 8 | 150.9 | 4,096 | 是 |

统计口径：p50 为 5 个样本的中位数，p95 为 nearest-rank（n=5 时等于第 5 个样本，即最大值）；`events/sec` 先按每个样本 `events / (wall_ms/1000)` 计算，再取中位数；`db_bytes` 取中位数。

### 9.3 观察

- 10 节点 DAG 在 `max_parallel=4` 下 p50 为 223 ms，单节点场景 p50 为 110 ms；相对“10×单节点”的朴素串行上界约 4.9× 加速。该数字包含固定启动/建库成本，不作严格线性扩展结论。
- 重试场景 p50 156 ms、10 条事件，与“1 次可重试失败后成功”的预期一致；没有出现 `node.failed` 重复。
- 取消场景 p50 53 ms，全部样本 `ok:true`；取消延迟的细粒度分解未单独埋点。
- `s6-cancel` 的 `db_bytes=4,096` 是未 checkpoint 的 WAL 未计入主文件导致，属已知统计口径问题，不代表数据未落盘。
- 所有 36 次运行（6 场景 × 6 次）均 `ok:true`，无场景退化。

### 9.4 结论

在当前机器和 release 构建下，harness 的 6 个固定场景均稳定通过；并行 DAG、重试、预算、取消和 handoff 路径没有出现错误或事件重复。该结果作为 H10 的正式基线，后续改动若使任一场景 p50/p95 显著恶化或 `ok` 变为 false，应视为性能/正确性回归。
