# Hive 0.1.1 发布与迁移手册

本文档面向 Windows x86_64 单机安装。发布包是工程预览版，不包含模型凭据、用户数据或自动更新。

## 1. 产物与版本

| 项 | 值 |
|---|---|
| 版本 | `0.1.1`（唯一来源：根 `Cargo.toml` 的 `[workspace.package] version`） |
| 平台 | Windows x86_64 |
| 暂存目录 | `dist/hive-0.1.1-windows-x86_64/` |
| 发布包 | `dist/hive-0.1.1-windows-x86_64.zip` |
| 迁移目标 | SQLite schema V6 |
| 二进制 | `bin/hive.exe`、`bin/hive-server.exe` |
| 包内文件 | 共 8 项：上述两个二进制、`hive.env.example`、`README.txt`、`RELEASE.md`、`CHANGELOG.md`、`start-hive.ps1`、`SHA256SUMS.txt` |

`scripts/package.ps1` 会构建 release 二进制、校验 `hive --version` 与目录/zip 名称一致、生成逐文件 SHA256 和 zip SHA256。包内 `RELEASE.md` 末尾追加实际构建 commit、构建时间与平台。

0.1.1 相对 0.1.0 不改 schema：`retention.json` 是数据目录下的独立 sidecar，旧库直接启动即可。升级只覆盖程序文件，保留 `data/`、`sandbox/` 与 `.env`。

## 2. 安装

发布包用户不需要 Rust，也不需要运行安装脚本。将 zip 解压到任意目录，例如 `C:\Hive`，然后进入解压出的 `hive-0.1.1-windows-x86_64` 目录：

```powershell
.\bin\hive.exe doctor
```

发布包内没有 `scripts/install.ps1`；它只是仓库内/开发者使用的 zip 安装 helper，不进入 zip，也不是用户安装步骤。

仓库内可选 helper：

```powershell
pwsh -NoProfile -File scripts/install.ps1 `
  -TargetDir C:\Hive `
  -From dist\hive-0.1.1-windows-x86_64.zip
```

- 目标目录非空时必须显式加 `-Force`；脚本只覆盖发布包文件，不删除用户已有 `data/`、`sandbox/` 或 `.env`。
- 脚本不修改 PATH、不写注册表、不需要管理员权限。
- 缺少 `SHA256SUMS.txt`、清单外文件或哈希不匹配时拒绝安装。
- 安装后打印 `.\bin\hive.exe doctor` 与启动命令。

## 3. 配置

1. 将 `hive.env.example` 复制为安装目录下的 `.env`。
2. 按需填写 `HIVE_API_KEY` 与 `HIVE_MODEL`；两者都非空才进入 live 模式，否则为 demo 模式。
3. 进程环境变量优先于 `.env`，`.env` 优先于内置默认值。
4. `.env` 中的未知 `HIVE_*` 键会返回字段名与行号；非 `HIVE_*` 键忽略。
5. `hive config migrate --dry-run [--file <path>]` 只报告键数量、缺失默认项、未知键和 API Key `set/unset`，不写文件，也不打印密钥值。

已知键：`HIVE_BASE_URL`、`HIVE_API_KEY`、`HIVE_MODEL`、`HIVE_HOST`、`HIVE_PORT`、`HIVE_MAX_PARALLEL`、`HIVE_ALLOW_SHELL`、`HIVE_DATA_DIR`、`HIVE_SANDBOX_DIR`、`HIVE_RETENTION_DAYS`、`HIVE_ARCHIVE_DIR`。`HIVE_RETENTION_DAYS` 默认 `30`，`0` 关闭自动归档；`HIVE_ARCHIVE_DIR` 未设置时默认 `<data_dir>/archive`，显式空值是配置错误。

默认监听 `127.0.0.1:8765`，数据目录 `data/`，沙箱目录 `sandbox/`，归档目录 `<data_dir>/archive`。相对路径按安装目录解析。

## 4. 启动

在安装目录运行：

```powershell
.\start-hive.ps1
```

启动脚本只切换目录并启动 `bin\hive-server.exe`，不修改环境。也可以直接运行：

```powershell
.\bin\hive-server.exe
```

最小健康检查：

```powershell
curl.exe --noproxy '*' http://127.0.0.1:8765/api/health
curl.exe --noproxy '*' http://127.0.0.1:8765/api/state
```

## 5. 备份

- 服务启动时若检测到 `hive.db` 的 `user_version < 6`，在 `Store::open` 前写入 `data\backups\hive.db.bak-v<旧版本>-<时间戳>`。
- 备份使用 SQLite 一致副本；备份失败时服务拒绝启动（fail-closed）。
- 已是 V6 时不自动备份；`user_version > 6` 时拒绝启动。
- 迁移 dry-run：`.\hive.exe db migrate --dry-run --db data\hive.db`。它使用临时一致副本迁移，原库字节不变，成功后删除副本。
- 发布前人工备份建议：停服后复制 `data\hive.db` 到独立位置，并在文件名中记录当前发布包版本。
- 不要在 WAL 模式下只复制 `hive.db` 单文件；必须使用服务备份、`db migrate --dry-run` 的内部一致副本，或停服并确认 checkpoint 完成。

## 6. 升级

0.1.0 → 0.1.1 不需要数据库迁移，但保留 `data/` 是必须的；`retention.json` 会在首次启动后出现在数据目录。

1. 停服，确认 `18765` 等测试端口或生产端口不再监听。
2. 记录当前版本与 `data\hive.db` 的 SHA256。
3. 解压新发布包并覆盖程序文件（至少 `bin\`），保留 `data\`、`sandbox\`、`.env` 与用户文件。仓库内开发者可用 `scripts/install.ps1 -TargetDir <dir> -From <zip> -Force`；该脚本不在发布包内。
4. 启动服务；旧 schema 会先自动备份，再向前迁移。
5. 用 `/api/health` 检查 `schema_version` 与 `status`，再执行一次业务冒烟。

## 7. 回滚

1. 停服。
2. 用升级前备份覆盖 `data\hive.db`，例如：

```powershell
Copy-Item -LiteralPath data\backups\hive.db.bak-v5-<timestamp> -Destination data\hive.db -Force
```

3. 退回到之前的发布包并启动。

迁移只向前，不提供 down migration。若没有可用备份，不能安全回滚到旧 schema；不要手工修改 `PRAGMA user_version` 冒充回滚。

## 8. SHA256 复算

```powershell
Get-FileHash -Algorithm SHA256 .\bin\hive.exe
Get-FileHash -Algorithm SHA256 .\bin\hive-server.exe
Get-FileHash -Algorithm SHA256 .\dist\hive-0.1.1-windows-x86_64.zip
```

- 包内 `SHA256SUMS.txt` 使用 `<hash>  <relative/path>` 格式。
- zip 旁的 `.sha256` 使用 `<zip hash>  <zip name>` 格式。
- 仓库内 `scripts/install.ps1` 在复制前逐文件校验 `SHA256SUMS.txt`；清单缺失、清单外文件或哈希不匹配都会拒绝安装。

## 9. 发布清单

| 项 | 内容 |
|---|---|
| 产物 | `hive-0.1.1-windows-x86_64.zip` 与同名 `.sha256` |
| 版本 | `0.1.1`；`hive --version` 必须输出 `Hive 0.1.1` |
| 构建 commit | 包内 `RELEASE.md` 的“本包构建信息”；源码提交见 `git log` |
| 迁移版本 | V6（`hive-store::SCHEMA_VERSION`） |
| 已知限制 | 单机单用户；仅 Windows x86_64 验证；无自动更新/签名/云发布；迁移不可回滚；`fs.write` 残余替换窗口；显式 `keep_dirty=false` 的 worktree 删除不可恢复；未配置凭据时为 demo 模式 |
| 验证人 | H11 实施对话；发布门由 H00 复核 |

## 10. 干净演练记录（0.1.0，历史）

日期：2026-11-24。使用 `dist/hive-0.1.0-windows-x86_64.zip`；zip SHA256 以同目录 `.sha256` 为准。本节保留 0.1.0 的演练证据，0.1.1 记录见第 11 节。

### 安装与 doctor

按发布包用户路径手动解压，不调用 `scripts/install.ps1`：

```powershell
Expand-Archive -LiteralPath dist\hive-0.1.0-windows-x86_64.zip `
  -DestinationPath work\h11-clean-r2 -Force
cd work\h11-clean-r2\hive-0.1.0-windows-x86_64
.\bin\hive.exe doctor
```

输出摘要：解压得到 8 项包内文件；无 Rust PATH 下 `bin\hive.exe doctor` 退出码 0，`rustc`/`cargo` 均为 `missing from PATH [optional]`，数据与沙箱目录均为 `writable`。

### 启动与 API

```powershell
$env:HIVE_HOST='127.0.0.1'
$env:HIVE_PORT='18765'
$env:HIVE_DATA_DIR='<repo>\work\h11-clean-r2\data'
$env:HIVE_SANDBOX_DIR='<repo>\work\h11-clean-r2\sandbox'
.\start-hive.ps1
```

使用 `curl.exe --noproxy '*'` 验证，避免系统代理拦截 localhost：

- `GET /api/health` → HTTP 200，`schema_version=6`，`status=ready`，`mode=demo`；`model_catalog=unavailable` 是因为未配置模型凭据。
- `POST /api/missions` → HTTP 201，返回 `mission.id=m_...`，`last_seq=1`。
- `GET /api/state` → HTTP 200，`last_seq=1`，`stats.missions=1`，`stats.events=1`。
- `GET /api/events?after_seq=0`（`Accept: text/event-stream`）→ 收到 `id: 1`、`event: mission.created`；`curl --max-time 2` 以退出码 28 结束，属于保持连接后的预期超时。

停止进程树后复核：PID 已退出，`Get-NetTCPConnection -LocalPort 18765 -State Listen` 返回 0 条，未留下监听。

### 配置与数据库 dry-run

- `hive config migrate --dry-run --file hive.env.example` → `Recognized keys: 9`，`API Key: unset`，`Validation: ok`，退出码 0。
- 在含测试 API Key 的 `drill.env` 上运行 → `Recognized keys: 3`，`API Key: set`，输出未包含密钥值（程序化检查 `contains_secret=False`），退出码 0。
- `hive db migrate --dry-run --db data\hive.db`（服务已停止的真实库）→ `Current version: 6`，`Pending migrations: none`，`Final version: 6`，临时副本已删除，退出码 0。
- 原库 SHA256 前后一致（`sha256_before == sha256_after`）。
- 对照观察：只复制 `hive.db` 单文件、丢掉 `hive.db-wal` 时，副本会被读成版本 0；这证明 WAL 模式下必须使用一致副本，不能按文件直接复制。

### 安装安全

- 目标目录非空且不带 `-Force` → 脚本抛错并退出 1，未写入文件。
- 带 `-Force` 重装 → 退出 0；用户已有 `.env`（`HIVE_PORT=18765`）与 `data\user-marker.txt`（`keep-me`）均保留。
- 从包副本删除 `SHA256SUMS.txt` 后运行 `scripts/install.ps1` → 退出 1，错误包含 `SHA256SUMS.txt missing; refusing to install`。
- 在包副本加入未列入清单的 `extra.txt` 后运行 `scripts/install.ps1` → 退出 1，错误包含 `file is not listed in SHA256SUMS.txt: extra.txt`。

## 11. 干净演练记录（0.1.1）

日期：2026-11-24。使用候选包 `dist/hive-0.1.1-windows-x86_64.zip`，SHA256 `c8c0d0b1e47377f450c61a85b89cdf20266b36a68e1723ec7d402aef817c6124`。本节写入后按同一源码重建最终包，二进制字节不变；最终包 SHA256 以同目录 `.sha256` 为准。

### 解压与 doctor

按发布包用户路径解压，不调用 `scripts/install.ps1`：

```powershell
Expand-Archive -LiteralPath dist\hive-0.1.1-windows-x86_64.zip `
  -DestinationPath work\h11-clean-011b -Force
cd work\h11-clean-011b\hive-0.1.1-windows-x86_64
.\bin\hive.exe doctor
```

- 包内 8 项文件齐全，`SHA256SUMS.txt` 逐文件复算 `BAD=0`。
- `hive --version` → `Hive 0.1.1`，退出码 0。
- PATH 只保留 `System32` 与 Windows 目录时，`doctor` 退出码 0；`rustc`/`cargo` 均为 `missing from PATH [optional]`，数据与沙箱目录均为 `writable`。
- `hive config migrate --dry-run --file hive.env.example` → `Recognized keys: 10`，`Missing defaults: HIVE_ARCHIVE_DIR`，`Validation: ok`，退出码 0。
- 演练中发现样例把 `HIVE_ARCHIVE_DIR` 写成空值，配置层会按严格校验拒绝；已改为注释掉的可选键，并复验通过。空值不是“使用默认值”。

### 启动、Mission 与 SSE

```powershell
$env:HIVE_HOST='127.0.0.1'
$env:HIVE_PORT='18766'
$env:HIVE_DATA_DIR='<repo>\work\h11-clean-011b\data'
$env:HIVE_SANDBOX_DIR='<repo>\work\h11-clean-011b\sandbox'
$env:HIVE_RETENTION_DAYS='1'
.\bin\hive-server.exe
```

使用 `curl.exe --noproxy '*'` 验证：

- `GET /api/health` → HTTP 200，`schema_version=6`，`status=ready`，`mode=demo`，`retained_since_seq=0`；`model_catalog=unavailable` 是未配置模型凭据的预期结果。
- `POST /api/missions` → HTTP 201，`mission.id=m_c0ca08a7-332f-45fa-9f09-9032f55d46e1`，`last_seq=1`。
- `GET /api/state` → HTTP 200，`stats.missions=1`，`stats.events=1`，`last_seq=1`。
- `GET /api/events?after_seq=0`（`Accept: text/event-stream`）→ 收到 `id: 1`、`event: mission.created`；`curl --max-time 2` 以退出码 28 结束，属于保持连接后的预期超时。
- 停止进程树后 PID 已退出，18766 端口无监听。

### 保留水位与重启

为在发布包上真实验证启动归档，停服后用 SQLite 把该 Mission 置为终态并回拨 `updated_at_ms` 两天（仅演练数据，不代表正常业务状态）：

```sql
UPDATE missions SET state='done', updated_at_ms=<now-2d> WHERE mission_id='m_c0ca08a7-...';
```

用同一数据目录、`HIVE_RETENTION_DAYS=1` 在 18767 重启：

- stderr：`startup retention archived 1 mission(s), 1 event(s)`。
- `data\retention.json` → `{"retained_since_seq":2}`。
- `data\archive\m_c0ca08a7-...\events.jsonl` 首行 header 的 `event_count=1`，后续 1 行事件。
- `GET /api/health` → HTTP 200，`retained_since_seq=2`。
- `GET /api/events?after_seq=0`（`Accept: text/event-stream`）→ HTTP 409，响应 `{"after_seq":0,"retained_since_seq":2,"snapshot_required":true}`。

再次停服并在 18768 重启：

- stderr 只有监听行，没有重复归档；重复周期归档保持幂等。
- `retention.json` 仍为 `{"retained_since_seq":2}`，health 仍报 2，`after_seq=0` 仍返回 409。
- 停止后 18767/18768 均无监听。

### 数据库 dry-run

- `hive db migrate --dry-run --db data\hive.db` → `Current version: 6`，`Pending migrations: none`，`Final version: 6`，临时副本已删除，退出码 0。
- 连续两次 dry-run 前后 `hive.db` SHA256 一致。

### 未复跑项

- 安装 helper 的非空目录、缺清单、清单外文件拒绝路径未在 0.1.1 复跑；0.1.1 未改 `scripts/install.ps1`，沿用第 10 节 0.1.0 证据。
- 未使用真实模型凭据做 live 冒烟；发布包默认 demo 模式。
