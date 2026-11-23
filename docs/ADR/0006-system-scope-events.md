# ADR 0006：系统作用域事件

- 状态：accepted
- 日期：2026-11-23
- 输入：H09b 的 Routine 事件落地、R08 审计对事件归属的复核
- 影响：`PROTOCOL.md` §3/§4、`hive-store`、`hive-server`、后续可观测与保留阶段

## 背景

V1 `events` 表的 `mission_id` 是 `NOT NULL`，而 `routine.updated`、`routine.deleted`、
`routine.error` 没有单一 Mission 所有者：更新和删除可能从未触发过 Mission，错误也可能发生在
创建 Mission 之前。强行把这类事件挂到最近一次 Routine 触发的 Mission 会污染该 Mission 的事件流；
新建一条假的 Mission 行又会让它出现在 Mission API、调度器和保留统计中。

H09b 因此在实现中使用固定 MissionId 作为进程级作用域。该选择不能停留在隐含实现细节，必须成为
可审计的协议规则。

## 决策

### D1 保留作用域

保留以下合法 v4 MissionId 作为系统作用域：

```text
m_00000000-0000-4000-8000-000000000000
```

它不是一条 `missions` 行，不得被调度、查询为普通 Mission 或写入 `runs`/`nodes`。

### D2 事件归属

- `routine.triggered` 使用本次触发创建的真实 MissionId。
- `routine.updated`、`routine.deleted`、`routine.error` 以及未来没有单一 Mission 所有者的进程级事件使用系统作用域。
- `mission_id` 在持久化事件中表示**作用域**；系统作用域是唯一允许不指向 `missions` 行的事件作用域。
- 有真实 Mission 所有者的事件不得使用系统作用域来逃避外键/归属检查。

### D3 客户端与保留

- 未按 Mission 过滤的事件流/SSE 必须包含系统作用域事件。
- 按普通 Mission 过滤的接口不把系统作用域伪装成可查询 Mission；需要全局过滤时由后续服务层追加，不改变 V1 schema。
- 保留/归档不得因为系统作用域没有 `missions` 行而静默丢弃其事件；系统作用域事件按全局审计记录保留。

### D4 代码约束

- 作用域 ID 必须有单一常量来源，禁止在业务代码重复拼接字符串。
- 普通 `MissionId::new()` 不得主动生成该保留值；UUID v4 的自然碰撞概率可忽略，但测试必须锁定其合法形状和固定值。
- 所有非触发类 Routine 事件必须有测试断言其作用域正确，且 `list_missions` 不出现该系统作用域。

## 后果

- 不修改 V1 schema，不引入假 Mission 行。
- `routine.updated/deleted/error` 可通过未过滤事件流观察；它们不会被误计入 Mission 聚合。
- 未来如果引入独立的全局事件表，可在不改动现有 Routine 语义的前提下迁移。
