# ADR 0003：API 兼容与旧数据迁移

- 状态：accepted
- 日期：2026-11-23

## 决定

1. 保留 `/api/*` 路径形状和首版核心端点，但 SSE 使用 v2 事件封套。
2. 不承诺旧 Python 前端无需修改即可运行；H08 必须同步升级 Web。
3. 首版不自动迁移 Python SQLite 数据库。
4. 提供可选的一次性导出：Mission 目标、用户消息、产物元数据和文件；旧 tasks/events 不映射为新 Run/Node。
5. 迁移前备份旧库，失败不修改源库。

## 原因

Python 的 `missions/tasks/events` 与新 `Mission/Run/Node` 层级没有无损映射；旧事件字段 `task_id/agent/content` 与新封套语义不同。模糊承诺兼容会迫使 H08/H11 返工。

## 代价

- 旧任务历史不能原样出现在新控制台。
- H11 需要提供导出工具和明确升级说明。

