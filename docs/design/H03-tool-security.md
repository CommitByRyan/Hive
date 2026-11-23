# H03 工具、审批与工作区安全设计

> 状态：实现设计候选；依据 ADR 0004 与协议 0.1，不修改冻结协议或存储 schema。
> 核心结论：所有工具只能经过 `validate -> policy -> approval -> execute -> audit`；默认拒绝，审批、执行和审计绑定同一不可变请求摘要。Windows 路径和 DNS/连接校验必须做到句柄/连接层，否则只能声明应用层限制。

## 1. 范围与统一入口

- `hive-policy` 负责纯策略判定、审批请求与决策；`hive-tools` 负责适配器、参数校验和执行；`hive-workspace` 负责已注册 Project 根目录及 worktree。
- H03 只消费 H02 的事务、事件和查询接口，不新增/修改迁移。`Approval`、`ToolCall` 的持久化模型由 H02 已有接口承载；缺接口时回报 H00，不在 H03 静默扩表。
- 每次调用生成服务端 `tc_<uuid>`；调用方提供的 `request_id` 只用于幂等查找。唯一约束建议为 `(session_id, request_id)`，重复且摘要相同返回原结果，不同则 `tool.request_conflict`。
- 固定流水线：解析严格 JSON -> JSON Schema/业务校验 -> 生成规范化请求 -> 策略判定 -> 必要时持久化审批 -> 执行前重新领取/校验 -> 写审计与结果事件。任何失败均返回稳定错误码并写拒绝审计；副作用执行前审计写失败则不得执行。
- 执行闸门同时验证 `tool_call_id`、`attempt_no`、当前 `lease_token`、审批 `request_hash` 与工具注册版本；任一变更为 `tool.stale_execution`。取消令牌在审批后、spawn/connect 前及执行中均检查。

## 2. 威胁模型与错误契约

错误响应只含稳定 `code`、安全 `message`、可公开 `details`；不返回堆栈、环境变量或凭据。下表是 H03 的稳定错误码候选，纳入公共类型前由 H00 确认。

| 攻击面 | 攻击方式 | 防护点 | 拒绝/失败错误码 |
|---|---|---|---|
| 路径穿越 | `../`、绝对路径、驱动器相对路径越界 | 拒绝歧义输入；真实路径/最近存在祖先规范化后比较 | `path.outside_workspace` |
| symlink/junction 逃逸 | 根内链接指向根外，检查后替换目录 | 解析 reparse point；打开后核对最终句柄路径；禁止跟随最终重parse点写入 | `path.reparse_escape` |
| Windows 8.3 短名 | `PROGRA~1`、短名绕过字符串前缀 | Windows `GetFinalPathNameByHandleW` 取得规范最终路径 | `path.outside_workspace` |
| UNC/设备路径 | `\\server\share`、`\\?\`、`\\.\`、驱动器混用 | 只接受已注册本地根；UNC 默认拒绝；按 volume+规范路径比较 | `path.unsupported_form` |
| ADS 数据流 | `file.txt:secret` 写隐藏流 | 拒绝流分隔符与非法 Windows 名称；只允许普通文件/目录句柄 | `path.ads_forbidden` |
| 命令注入 | 拼接命令、`cmd /c`、PowerShell 内联脚本 | argv 数组；默认禁 Shell；白名单程序直接 spawn，不经解释器 | `shell.disabled` 或 `shell.program_denied` |
| 参数走私 | `--` 后注入、重复参数、参数展开、白名单程序危险子命令 | 严格 Schema+逐工具参数策略；不自动展开；按子命令授权 | `tool.arguments_denied` |
| 环境变量泄漏 | 子进程读取 `*_KEY`、代理凭据或伪造 `PATH` | 默认空环境；仅注入明确变量；绝对程序路径；日志脱敏 | `tool.environment_denied` |
| DNS rebinding | 校验时公网、连接时变为内网 | 一次性解析并校验全部地址；socket 直连已校验 IP，不复用解析器 | `network.address_denied` |
| 本地端口 SSRF | 公网域名解析到 `127.0.0.1`、访问元数据/管理端口 | IP+端口双重 allowlist；禁 loopback/link-local/私网/组播/未指定地址 | `network.endpoint_denied` |
| 审批重放/替换 | 重用 approved、改参数、旧 attempt 决策 | 单次消费；原子 pending 比较；绑定请求摘要、attempt、lease、审批人 | `approval.replayed` 或 `tool.stale_execution` |
| 并发 TOCTOU | 检查与打开间换链接、并发决定审批、DNS 再变化 | 句柄级复核、单事务 CAS、连接级 IP 固定；失败关闭而非继续 | `tool.validation_changed` |

补充边界：不可信输出仅作为带类型的数据，不能反向修改工具策略；进程内路径校验不是 OS 沙箱，恶意白名单程序仍可越权。未实现 OS 隔离时 UI/doctor 必须显示“应用层限制”。

## 3. Windows 优先的路径策略

### 3.1 输入与规范化算法

1. 注册 Project 时打开根目录句柄，以最终路径建立 `WorkspaceRoot { root_id, canonical_root, volume, file_index/volume_serial }`；运行时拒绝根已替换。根路径保存为 `VerbatimDisk` 形式用于 Win32 比较，不把用户路径直接转成 verbatim 来绕过校验。
2. 输入必须是 UTF-8 相对路径；先拒绝 NUL、ADS 冒号、空段之外的 `.`、任何 `..`、绝对/UNC/设备/驱动器相对形式，以及尾随点/空格、保留 DOS 名称。这样不会把 `C:foo` 当相对安全路径。
3. 对读操作：从根逐段打开；每段用 `FILE_FLAG_OPEN_REPARSE_POINT` 检查。允许链接时必须解析其最终目标并重新从第 1 步验证；目标不存在、跨卷、重parse循环均拒绝。对目录只接受目录句柄，对文件只接受普通文件。
4. 对“不存在目标文件”的写入：先词法计算父目录；从最长已存在祖先开始做最终路径规范化，并验证它仍在根内；逐段创建剩余目录，每创建一段立即打开并复核，禁止在存在重parse点的路径中创建。最后以“新建且不可跟随链接”方式打开叶文件，再用最终句柄路径复核。
5. 最终比较使用 Windows 不区分大小写的规范路径（优先比较最终句柄路径与根句柄，而非调用者字符串）；边界是目录段，`C:\root2` 不属于 `C:\root`。Linux 使用字节/平台语义，不全局 lowercase。
6. 所有打开都用根目录句柄/相对句柄遍历为最终目标。若首版只能使用 `std::fs::canonicalize + 打开后复核`，则标记 TOCTOU 限制，不能宣称阻止恶意并发替换。

```rust
pub struct PathPolicy { roots: Vec<WorkspaceRoot>, allow_links: bool }
pub enum PathAccess { Read, CreateFile, CreateDir, Delete }
pub struct AuthorizedPath {
    root_id: PrjId,
    relative_path: PathBuf,
    final_path: PathBuf,
    handle: HandleGuard, // 验证与使用同一句柄，关闭时释放
}

impl PathPolicy {
    pub fn authorize(&self, root: &PrjId, input: &Utf8Path,
                     access: PathAccess) -> Result<AuthorizedPath, ToolError>;
    fn validate_relative_lexical(&self, input: &Utf8Path) -> Result<(), ToolError>;
    fn open_checked(&self, root: &WorkspaceRoot, relative: &Utf8Path,
                    access: PathAccess) -> Result<HandleGuard, ToolError>;
}
```

关键伪代码：`validate -> root handle -> walk/open each component without following -> verify final handle -> return handle`；文件工具只消费 `AuthorizedPath.handle`，禁止随后再按原字符串重新打开。删除还需再次确认对象类型及目录非重parse点，避免把链接目标当目录递归删除。

### 3.2 默认权限

- 默认仅允许注册根内的 `work/`、`outputs/` 和显式授权源码路径；`.hive/`、`.git/`、凭据文件默认禁读写。写入路径策略与 Project 角色绑定，不因审批通过而扩大根范围。
- 审批只能允许本次明确请求，不能关闭根边界、ADS/UNC 检查或把不存在路径永久加入白名单。

## 4. 工具注册表与请求摘要

```rust
pub struct ToolSpec {
    pub name: String,                 // 稳定、全局唯一，如 fs.read
    pub version: u32,                 // 参数/行为变化递增
    pub arguments_schema: serde_json::Value, // JSON Schema draft 2020-12
    pub risk: RiskLevel,              // ReadOnly | Write | Execute | Network
    pub approval: ApprovalMode,       // Never | Policy | Always
    pub timeout: Duration,
    pub max_output_bytes: usize,      // 含 stdout+stderr 各自上限
    pub side_effects: bool,
    pub enabled_by_default: bool,
}
pub struct NormalizedToolRequest {
    pub tool_call_id: ToolCallId, pub tool_name: String, pub tool_version: u32,
    pub session_id: SessionId, pub node_id: NodeId, pub attempt_no: u64,
    pub lease_token: LeaseToken, pub arguments: serde_json::Value,
    pub request_hash: [u8; 32],
}
```

- 注册表在进程启动时构建并冻结；同名同版本重复注册直接启动失败，模型不得动态注册工具。
- Schema 默认 `additionalProperties: false`，限制字符串长度、数组项数、对象深度、数值范围和整体请求字节数；先限长再解析，防止资源消耗。
- `request_hash = SHA-256(canonical_json({tool_name, version, args, mission/run/node/session, attempt_no, lease_token, policy_version}))`；规范化 JSON 使用固定键序，不把超时时间、随机 ID 等易变字段混入审批内容。
- `Policy` 模式按风险、路径/地址和角色能力决定；`Always` 不可被配置降级。结果分 `stdout/stderr` 截断，记录原始长度、SHA-256 和存储引用；事件只放摘要。

## 5. 审批状态机与 Node 解耦

```text
pending --唯一有效决策--> approved
   |------拒绝----------> rejected
   |------到期/超时扫描-> expired
   +------Mission/Node/Run 取消--> cancelled
```

- `pending` 持久化包含 `approval_id`、完整关联 ID、`tool_call_id`、`request_hash`、规范化参数的安全展示、`expires_at=now+600s`、`policy_version` 和 `attempt_no`。创建审批、Node 转 `waiting_approval`、写 `node.approval_requested` 必须同事务；事件 payload 含 `approval_id`。
- 审批等待不持有调度 worker、全局锁、模型连接或兄弟节点配额。该 Node 让出执行槽，其他 ready Node 继续；Mission 仅在没有可运行 Node 且存在等待审批时呈现派生 `waiting_approval`。
- `approve/reject` 使用单事务 `UPDATE ... WHERE approval_id=? AND state='pending'`；影响行数非 1 返回 `approval.conflict`。到期判断使用数据库同一时钟：决策时若 `now >= expires_at`，同事务写 `expired`，不得批准。
- approved 不是执行授权缓存：仅可被匹配的 `tool_call_id + request_hash + attempt_no + lease_token` 消费一次。恢复时重新读取 pending 和 Node 关联；重启不自动决定。Node 恢复后回到 `running`，重新经过执行闸门；参数变化必须新建审批。
- 取消 Mission/Run/Node 时，同事务取消关联 pending、写 `node.approval_decided`（decision=cancelled）并唤醒等待者。审批失败/过期使当前工具调用失败，由节点错误/重试策略处理，不直接篡改兄弟分支。
- 审批人标识和来源必须记录；H08 提供认证前不得把“本地 HTTP 请求”视为可信审批人。重复决定返回冲突而非 200 成功。

## 6. Shell 与子进程

- `shell.enabled=false` 是默认值；启用仍逐次审批，且只允许配置中的**规范化绝对程序路径**。默认禁止 `cmd`、`powershell/pwsh`、`bash/sh`、解释器及可任意执行子命令的通用构建工具；确需开放按 `(program, subcommand, args schema)` 单独授权。
- 使用 argv 数组直接 spawn，永不拼接命令字符串，不经过 Shell，不做 `%VAR%`、`$VAR`、glob 或用户主目录展开。程序路径校验白名单，参数另做 allow/deny 规则，不能仅按 basename。
- `cwd` 必须经路径授权；环境默认清空，仅注入最小无密钥变量（如固定 `PATH`、locale）；日志不记录敏感值。stdin 默认关闭，stdout/stderr 有界读取，达到上限终止或明确截断，避免内存/磁盘耗尽。
- Windows 使用 Job Object（`KILL_ON_JOB_CLOSE` + breakaway 限制），超时/取消/租约失效时终止整个进程树；Unix 使用独立进程组并杀组。等待退出并收集输出，超时返回 `shell.timeout`，清理失败返回 `shell.cleanup_failed`，不能假报成功。
- Shell 属不可幂等副作用：持久化 `intent -> started -> completed/failed`，进程崩溃后若 `started` 无终态则 Node 进 `needs_recovery`，禁止自动重放。该能力仍是应用层限制，不是 OS 沙箱。

## 7. 网络策略

- 默认拒绝所有工具网络访问；仅启用具名 egress profile：`host 规则 + 端口 + scheme + 最大响应字节 + timeout`。域名精确匹配或有边界的后缀匹配（`example.com` 不匹配 `badexample.com`）；不默认接受通配符和 CIDR。
- 拒绝 URL userinfo、非 http/https、控制字符和歧义主机；IDNA 规范化后匹配。域名与解析 IP **均校验**：任一 A/AAAA 为 loopback、private、link-local、multicast、unspecified、广播、文档/保留或禁止网段，整次请求拒绝。IPv4-mapped IPv6 先映射再分类。
- 默认禁 loopback/link-local/私网，仅显式 profile 可放行且仍限端口。Magpie `127.0.0.1:3425` 是 `hive-llm` 的专用配置，不自动授予通用 HTTP 工具。
- 对每次请求解析一次并验证全部地址，仅尝试已验证地址；连接 socket 时直接连接选定 IP，TLS SNI/Host 保持原域名。连接后记录实际 peer IP 并复核。不得让 HTTP 客户端再次 DNS 解析。
- 默认不重定向；如启用，仅允许有限跳数，并对每跳完整执行 scheme/host/DNS/IP/port 校验和审批策略。连接复用池以 `(规范主机, 已验证 IP, port, TLS 配置, profile)` 隔离，禁止跨 profile 复用。
- 有界流式读取、解压后大小限制、连接/读/总超时；不转发调用方任意代理、Cookie、Authorization。若使用现成客户端未能固定 peer IP，能力声明必须为“应用层 SSRF 限制，未防 DNS rebinding”。

## 8. 审计记录与完整性

每次工具调用至少记录：`event_id/seq`、`trace_id`、Mission/Run/Node/Session、`tool_call_id`、`attempt_no`、`lease_token`、工具名/版本、策略版本、规范化参数摘要及必要安全快照、审批 ID/决定者/决定时间、开始/结束时间、结果码、退出码、截断标志、输出长度/SHA-256/存储引用、取消原因。网络另记规范主机、选中/实际 peer IP、端口；文件另记 root ID、相对路径、最终路径哈希和文件身份；Shell 另记程序哈希、argv 的安全表示和进程树清理结果。

- `node.tool_call` 表示 intent，`node.tool_result` 表示完成摘要；`started` 必须写实际 OS spawn/connect 已发生的事实。意图、开始和结束都走 H02 单写事务；事件持久化成功后才广播。
- 审计表只追加，应用角色禁止 UPDATE/DELETE；每条含 `prev_hash` 和 `entry_hash=SHA256(canonical_record || prev_hash)`，使用数据库外密钥定期签名/锚定检查点，验证器独立只读校验。密钥不得进事件、日志或仓库。
- 哈希链只能提供**篡改可检测**，不能抵御本机管理员同时改库和密钥；交付文档不得写“绝对不可篡改”。保留策略、导出和时间同步由 H09/H11 接管，H03 不自行删审计。
- 参数和输出可能含密钥：审计优先存规范化摘要与显式脱敏快照，不明文记录环境、Authorization、Cookie。完整输出单独有界存储，访问受相同权限控制。

## 9. 实现顺序与模块边界

1. `hive-policy`：错误码、纯路径/网络分类函数、注册表策略与审批决策接口；测试不依赖 OS 副作用。
2. `hive-tools`：统一 dispatcher、严格参数校验、只读文件/有界 HTTP/禁用 Shell 适配器；无副作用 mock executor 验证流水线顺序。
3. `hive-workspace`：Windows 句柄遍历与进程 Job Object；平台能力由 trait 隔离，非 Windows 构建明确能力等级。
4. 接 H02 持久化：审批 CAS/恢复、工具副作用三段事件和审计校验；最后接 H04/H05 的取消、租约和并行调度，不在 H03 实现调度器。

## 10. H03 验收清单（测试名 -> 断言）

| 测试 | 具体断言 |
|---|---|
| `policy_rejects_parent_traversal` | `../outside` 返回 `path.outside_workspace`，根外无创建/修改 |
| `policy_rejects_short_name_and_case_alias` | `ROOT~1`、大小写变体均不能越过段边界；根内正常文件仍可读 |
| `policy_rejects_junction_symlink_escape` | 根内 junction/symlink 指向外部时拒绝，外部文件内容不返回 |
| `policy_rejects_unc_ads_device_paths` | UNC、`\\.\`、ADS、`C:foo` 返回指定路径错误，不触发 I/O |
| `create_missing_file_checks_parent_and_leaf` | 不存在的新文件可在根内创建；父链接/叶链接交换均拒绝并清理临时文件 |
| `open_handle_detects_toctou_swap` | 校验后替换目录时，句柄复核拒绝；不得使用原路径继续写入 |
| `registry_rejects_unknown_and_extra_args` | 未知工具/额外字段/超长输入在 execute 前拒绝，executor 调用数为 0 |
| `approval_expires_and_replay_conflicts` | 600s 边界 expired；重复批准/批准后改参数返回冲突，execute 仅 0 次 |
| `approval_survives_restart_without_auto_execute` | 重启后 pending 保持；未决策不执行，合法决策只消费一次 |
| `waiting_node_does_not_block_sibling` | A 等审批时 B 成功完成；无全局锁/worker 占用，Mission 派生态正确 |
| `stale_attempt_cannot_use_approval` | attempt 或 lease 改变后 approved 不可复用，返回 `tool.stale_execution` |
| `shell_disabled_and_argv_not_interpreted` | 默认 Shell 拒绝；启用后 `; & $()` 仅作为字面 argv，无第二进程副作用 |
| `shell_timeout_kills_descendants_and_truncates` | 超时后 Job/进程组无存活后代，stdout/stderr 有界且标截断 |
| `network_rebind_and_redirect_to_loopback_denied` | 公网域名二次解析/重定向至 loopback 无连接成功，实际 peer 记录为空或拒绝 |
| `network_blocks_private_metadata_and_bad_ports` | 私网、169.254.169.254、IPv4-mapped IPv6 与未放行端口均在 connect 前拒绝 |
| `audit_chain_detects_tamper_and_redacts` | 修改历史字节使校验失败；日志不含植入的 token；audit 写失败时 executor 未运行 |
| `uncertain_shell_completion_requires_recovery` | 崩溃保留 started 无 completed；重启进入 `needs_recovery`，不自动重放 |
| `cancellation_cancels_pending_and_execution` | 取消同事务终止 pending；运行中进程树/连接终止，迟到提交因 lease 失效被拒 |

Windows 用例必须在 NTFS 实机运行并包含需要权限时的明确跳过/失败报告；跳过 symlink 用例不得据此声称 Windows 验收完成。

## 11. 风险登记与复核分工

| 风险 | 最容易做错的点 | 必须复核 |
|---|---|---|
| Windows TOCTOU/重parse点 | 只做 canonicalize+前缀检查；创建不存在叶文件、并发换链接时仍逃逸 | `workbuddy/mimo-v2.6-pro` 专审 Win32 句柄、8.3/UNC/ADS/Job Object；`deepseek-v4-pro` 审接口不变式 |
| DNS 与 HTTP 客户端行为 | 校验 DNS 后又让客户端解析、连接池跨策略复用、重定向漏验 | `mimo-v2.6-pro` 审 socket/TLS/连接池；`deepseek-v4-pro` 审失败关闭与降级声明 |
| 审批与副作用恢复 | 审批未绑定 request/attempt/lease；状态事件非同事务；崩溃后重复 Shell | `workbuddy/kimi-k3.1-pro` 独立只读审状态机、CAS、取消/恢复及审计链，不能由实现者自审替代 |

H03 接受门：18 项测试均有真实结果，Windows 连接/句柄能力未实现时必须明确降级；不得以“有路径前缀检查”或“HTTP 校验域名”宣称完整安全边界。
