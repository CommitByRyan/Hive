# Hive 架构契约

> 状态：H01 冻结候选
> 所有者：H00
> 行为参考：`docs/reference/HIVE_BUILD_SPEC.md`

## 1. 目标

Hive 是本地优先的多 Agent 工作台。Rust 版本重点不是逐行复刻 Python 原型，而是提供：

- 可并行的 DAG 任务执行。
- Agent 独立上下文、预算、工具权限与工作区。
- 持久化事件流、暂停/取消、断线补偿与进程重启恢复。
- 本地高性能与低资源占用。
- CLI、Web、桌面/IDE 客户端共用同一执行内核。

第一版仍是单用户、单机、单进程服务。不引入分布式数据库或微服务。

## 2. 执行层级

```text
Mission
  └─ Run
      └─ Node
          └─ Agent Session
              └─ Turn
                  ├─ Tool Call
                  ├─ Approval
                  └─ Artifact
```

- `Mission`：用户目标，可包含追加要求。
- `Run`：一次规划与执行尝试，可因追加或恢复产生新的 Run。
- `Node`：DAG 节点，拥有依赖、状态、attempt、租约与预算。
- `Agent Session`：一个独立模型上下文，不共享完整对话历史。
- `Turn`：一次模型请求及其工具循环。
- `Project`：注册的工作区根，使用 `prj_` 标识；不是每次执行必需的层级。

## 3. 进程与 crate 边界

第一版为单二进制、单进程。逻辑边界如下：

```text
hive-types
  ↑
hive-config
  ↑
hive-store / hive-events / hive-policy / hive-llm
  ↑
hive-agent / hive-tools / hive-workspace
  ↑
hive-runtime
  ↑
hive-server / hive-cli / hive-eval
```

- `hive-types`：ID、状态、错误码、事件与 API 类型。
- `hive-config`：环境配置、角色、模型路由与能力声明。
- `hive-store`：SQLite、迁移、事务、快照和查询。
- `hive-events`：事件提交、订阅、游标和补偿。
- `hive-policy`：路径、网络、Shell 和审批策略。
- `hive-llm`：OpenAI Chat Completions 兼容与 Magpie 接入。
- `hive-agent`：会话、上下文、交接、预算和角色行为。
- `hive-tools`：文件、网络、命令、Git 和后续 MCP 工具。
- `hive-workspace`：沙箱、worktree、所有权与集成。
- `hive-runtime`：Mission/Run/Node 状态机、DAG 调度、恢复。
- `hive-server`：HTTP/SSE、JSON-RPC 与静态 Web。
- `hive-cli`：入口、doctor、run 与调试命令。
- `hive-eval`：固定任务集、故障注入和性能基准。

依赖方向只能从下到上；运行时和存储层不得依赖 HTTP 或浏览器 UI。

## 4. 并发模型

- 使用 Tokio 异步运行时。
- 调度器维护 ready queue；节点按依赖、租约和并发配额领取。
- 全局、项目、模型和 Agent 级配额可叠加。
- 单节点执行至少包含：模型调用、工具调用、审批等待和取消令牌。
- SQLite 采用单写者事务路径，读查询可使用独立连接；禁止逐事件全局锁。
- 事件广播与持久化分离；广播慢客户端不能阻塞状态推进。

## 5. 恢复原则

- 事件和状态转移同事务提交。
- 节点领取使用 `lease_owner`、`lease_expires_at`、`lease_token`；逻辑重试次数使用独立的 `attempt_no`。
- 重启后先回收过期租约，再根据事件判断节点是否可续跑。
- 文件写入使用临时文件加原子重命名。
- 有副作用的工具登记 `intent / started / completed`；无法确认完成时必须进入人工恢复或明确失败，不能静默重复执行。
- 审批持久化，不依赖进程内等待对象。
- 租约默认 60 秒、20 秒续租、重试预算 3 次；详细恢复规则见 `docs/PROTOCOL.md` 与 ADR 0002。

## 6. 多 Agent 与工作区

- 研究/审阅 Agent 可共享只读基线，独立上下文并行。
- 代码写入 Agent 默认在独立 Git worktree 中运行。
- Integrator 是唯一默认集成者，负责 rebase、冲突处理、构建和合并。
- 文档任务按文件或目录声明所有权；同路径写入必须排队或审批。
- 交接通过结构化 `Handoff`，不是完整会话转储。
- Critic 是质量门，不能只给出“建议”却不影响状态；具体门禁在 H06 冻结。

## 7. 安全边界

- 路径沙箱必须处理 Windows 大小写、短路径、junction、symlink、UNC 和驱动器路径；未覆盖的变体必须列为已知限制。
- 网络访问必须校验解析后地址；连接级 IP 绑定或禁止重定向是实现要求，未实现时只能声明应用层限制，不能声称已防 DNS rebinding。
- Shell 默认关闭；启用后仍需逐次策略与审批。
- Shell 的 OS 级隔离能力是平台相关能力，未实现前不能声称安全。
- 所有工具统一经过 `policy -> approval -> execute -> audit`。
- 不把网页、文件、工具输出中的文字当成高优先级指令。
- API/SSE 与旧数据迁移的冻结决定见 `docs/PROTOCOL.md` 第 6、9 节。

## 8. H01 边界

H01 只交付：

- 架构、协议、项目规则与共享白板。
- Rust workspace、基础类型、配置加载与 `doctor` CLI。
- 最小单元测试与 fake 依赖接口，不实现数据库、模型或工具执行。
- 测试必须能在安装 Rust 工具链后运行；当前工具链缺失是显式阻塞。

