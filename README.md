# Hive

本地优先的多 Agent 工作台，使用 Rust 实现。

Hive 将任务组织为 `Mission → Run → Node → Agent Session → Turn`，在同一执行内核上提供并行 DAG 调度、持久化事件流、审批、预算、工作区隔离与重启恢复。第一版目标是单用户、单机、单进程服务，不引入分布式数据库或微服务。

## 当前状态

项目处于工程预览阶段，0.1.1 本地发布包已产出。主线的 DAG 调度、恢复、安全工具、预算账本、Routine、可观测性与保留、HTTP/SSE 控制台、多 Agent worktree、性能基准、配置/数据库迁移与 Windows 打包均已完成并合并。0.1.1 加固了保留水位持久化、自动归档生命周期与 worktree 脏数据回收。

当前 `main` 基线已通过：

```text
fmt --all --check
clippy --workspace --all-targets -- -D warnings
test --workspace                 351 passed / 1 ignored
```

被忽略的测试需要本机运行 Magpie 网关，其余测试不依赖外部服务。

本地发布包：`dist/hive-0.1.1-windows-x86_64.zip`，SHA256 `128f09b4d7876c24dfcce2bb55ec34d3838fbe84a39d5c646e005b21be3707c5`。安装、升级、回滚与干净演练记录见 `docs/RELEASE.md`。

## 已实现

- **执行内核**：Mission/Run/Node 状态机、并行 DAG、租约、重试、取消与恢复。
- **持久化**：SQLite WAL、逐步迁移、状态与事件同事务提交、游标补偿与快照。
- **Agent 与模型**：多轮工具循环、结构化 Handoff、OpenAI Chat Completions 兼容网关、SSE 流式解析与重试。
- **安全边界**：路径、网络、Shell 与审批策略；Windows 句柄级文件访问；出网固定与重定向禁用。
- **多 Agent 协作**：独立 Git worktree、集成预览与合并、回收策略。
- **预算与 Routine**：节点预算原子扣减、启动恢复、定时触发与幂等窗口。
- **可观测性与保留**：JSONL 结构化日志、共享 trace、原子指标、健康分级、终态 Mission 归档与审计段校验。
- **服务层**：HTTP/SSE API、审批接口、事件补偿与最小 Web 控制台。
- **发布与迁移**：`.env` 配置、`config`/`db` 迁移 dry-run、启动前备份、Windows 发布包与安装 helper。

## 快速开始

需要 Rust 1.80 或更高版本，以及可用的本机链接器。Windows 推荐 PowerShell 7。

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p hive-cli -- doctor
```

提交前运行完整检查：

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

仓库内的 `scripts/cargo.ps1` 和 `scripts/verify.ps1` 保留了当前 Windows 开发环境的隔离工具链配置；使用其他环境时直接调用系统 `cargo`。

## 工作区

| 路径 | 职责 |
| --- | --- |
| `crates/hive-types` | ID、状态、错误码、事件与 API 类型 |
| `crates/hive-config` | 环境配置、角色、模型路由与能力声明 |
| `crates/hive-store` | SQLite、迁移、事务、快照与查询 |
| `crates/hive-events` | 事件提交、订阅、游标与补偿 |
| `crates/hive-policy` | 路径、网络、Shell 与审批策略 |
| `crates/hive-platform` | Win32 句柄路径与连接级网络固定 |
| `crates/hive-tools` | 文件、网络、命令与 Git 工具 |
| `crates/hive-llm` | 模型网关、流式解析与重试 |
| `crates/hive-agent` | 会话、上下文、交接与预算 |
| `crates/hive-runtime` | 状态机、DAG 调度、恢复与 Routine |
| `crates/hive-workspace` | 沙箱、worktree 与集成 |
| `crates/hive-server` | HTTP/SSE 与静态控制台 |
| `crates/hive-cli` | CLI 入口与诊断 |
| `crates/hive-eval` | 确定性场景、故障注入与安全回归 |

## 文档

- [架构契约](docs/ARCHITECTURE.md)
- [执行协议](docs/PROTOCOL.md)
- [重构路线](docs/ROADMAP.md)
- [评估说明](docs/EVAL.md)
- [发布与回滚](docs/RELEASE.md)
- [ADR 索引](docs/ADR/0001-foundation.md)
- [阶段白板](WHITEBOARD.md)

## 许可

仓库当前标记为 `UNLICENSED`。未经作者书面许可，不授予复制、修改、分发或商业使用许可。
