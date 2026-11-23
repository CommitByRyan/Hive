# ADR 0002：事件身份、状态机与恢复

- 状态：accepted
- 日期：2026-11-23

## 决定

1. `event_id` 使用 `e_<uuid>`，`seq` 是数据库内单调递增的 `u64`，作为唯一客户端游标。事件表以 `seq` 为主键、`event_id` 为唯一键。
2. Run 状态为 `created -> planning -> running -> done/failed/cancelled`，运行中可 `paused -> running`。
3. Node 状态增加 `failed_final` 与 `needs_recovery`；`failed -> retry_ready -> ready` 只在预算未耗尽时发生，预算耗尽进入 `failed_final`。
4. `attempt_no` 是逻辑执行次数；`lease_token` 是执行所有权，两者不得混用。
5. 默认租约 60 秒、20 秒续租、重试预算 3 次；配置可由角色或 Mission 覆盖，但语义固定。
6. 恢复时先回收过期租约，再检查 `intent/started/completed`。无法确认副作用的 Node 进入 `needs_recovery`，不得自动重放。
7. Mission 的 `waiting_approval` 是派生观测态；单个节点等待审批不阻塞其他分支。
8. Approval 状态为 `pending/approved/rejected/expired/cancelled`，默认 600 秒超时，pending 在重启后保留。

## 原因

R01 指出事件双 ID、Node 无终态失败、Run 状态缺失、attempt 语义混用和审批超时未冻结，会让 H02/H03/H05 各自发明规则。本 ADR 将不可逆决定前移到契约层。

## 代价

- 状态机比 Python 原型复杂，H02/H05 需要额外表和测试。
- 不能自动恢复所有中断任务；部分副作用必须人工决策。
- `seq` 是本地库顺序，未来分布式扩展需要新的全局排序机制。

