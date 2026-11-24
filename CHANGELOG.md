# Changelog

## 0.1.0 — 2026-11-24

首个工程预览版本，面向单机、单用户、单进程场景。

### 可交付能力

- 执行内核：Mission → Run → Node → Agent Session → Turn，DAG 调度、租约、重试、取消与重启恢复。
- 持久化与事件：SQLite WAL、V1–V6 向前迁移、状态与事件同事务、SSE 游标补偿与快照。
- 模型与 Agent：OpenAI Chat Completions 兼容网关、SSE 解析、结构化 Handoff、多轮工具循环。
- 安全边界：路径、网络、Shell、审批策略；Windows 句柄级文件访问与固定出网。
- 协作与工作区：独立 Git worktree、集成预览/合并/回收。
- 预算、Routine、可观测性与保留：节点预算、定时触发、JSONL 日志、trace、指标、健康分级、终态 Mission 归档。
- 服务层：HTTP/SSE API、审批接口、最小 Web 控制台。

### 配置、迁移与发布

- `.env` 支持：注释、空行、`KEY=VALUE`、`export`、单/双引号、UTF-8 BOM；进程环境变量 > `.env` > 默认值。
- `hive config migrate --dry-run`：只报告已识别键数量、缺失默认项、未知键与 API Key set/unset，不写文件。
- `hive db migrate --dry-run`：使用 `VACUUM INTO` 一致副本演练迁移，原库不变。
- 启动迁移备份：旧 schema 在 `Store::open` 前备份到 `data/backups/hive.db.bak-v<旧版本>-<时间戳>`，备份失败拒绝启动。
- 安装包语义的 `hive doctor`：只要求配置可加载、数据/沙箱目录可写；Rust 工具链为可选开发项。
- Windows x86_64 发布脚本：release 构建、暂存、逐文件 SHA256、zip 与 zip SHA256。
- 安装脚本：只复制/解压，不改 PATH、不写注册表、不需要管理员；保留用户已有 `data/` 与 `.env`。

### 已知限制

- 仅验证 Windows x86_64；非 Windows 回退不是句柄级路径隔离。
- 单机单用户；默认仅监听回环地址，未内置面向公网的身份认证与 TLS 终止。
- 不提供自动更新、代码签名、云发布或自动回滚。
- 迁移只向前，不提供 down migration；回滚必须依赖升级前备份。
- `fs.write` 仍有已记录的路径级原子替换残余窗口（`atomic_replace_handle_relative=false`）。
- 发布包不包含模型凭据；未配置 API Key/模型时以 demo 模式运行。
