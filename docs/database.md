# 数据库设计

先用轻量的 SQLite 开发,上云时换成 PostgreSQL 只改连接串。结构在 [`migrations/0001_schema.sql`](../migrations/0001_schema.sql),访问数据库的代码只在 `crates/trade-store`。表之间的关系对应 [`diagrams.md`](diagrams.md) 的图 8。

## 可移植规则

同一份 SQL 在两种数据库上行为一致,靠下面几条:

1. **只用三种列类型**:`TEXT`、`BIGINT`、`DOUBLE PRECISION`。布尔值存 `0` 和 `1`,时间存 Unix 秒,结构化的值存 JSON 文本。
2. **只用两边都支持的语法**:占位符 `$1`、`$2`;`INSERT ... ON CONFLICT ... DO UPDATE` 和 `DO NOTHING`;聚合结果显式 `CAST`(PostgreSQL 的 `SUM(BIGINT)` 返回 `NUMERIC`,不转回 `BIGINT` 就读不出来)。
3. **一个连接池服务两种数据库**:sqlx 的 `AnyPool`,按连接串选驱动。行用 `#[derive(FromRow)]` 映射成结构体,不逐列手写读取。
4. **外键两边都检查**:sqlx 打开 SQLite 连接时默认开启外键检查,本地测试就能发现插入顺序这类问题。
5. **并发靠版本号**:任务每次更新都带「版本号没变才更新」的条件,两种数据库都支持,不依赖数据库特有的锁语法。
6. **结构只通过迁移脚本改**:启动时自动执行 `migrations/` 下还没执行过的脚本。已经用过的脚本不能再改(sqlx 会校验内容),改结构就加一个新的 `0002_...sql`。

行情数据不放在这个库里。演示和测试用 `fixtures/market.json`,由行情适配器按「当天之前已公开」的规则读出;以后接真实行情源时换适配器,库不用动。

## 表

| 表 | 图 8 的实体 | 存什么 | 关键约束 |
|---|---|---|---|
| `task_types` | TASK_TYPE_PROFILE | 任务类型配置(JSON):工具的风险分级、上限、放权级别、等待期限 | 主键 `(task_type, version)`。同一版本的内容不能变 |
| `tasks` | TASK、AUTHORITY、WAIT | 任务和当前状态;执行身份和授权市场;当前等待的人、期限和到期动作;各项上限的用量 | `idempotency_trigger_key` 唯一;`version` 用于条件更新;外键指向 `task_types` |
| `task_events` | SESSION_EVENT、AUDIT_RECORD | 时间线和审计记录,只追加:每次状态变化、拒绝、批准决定、动作、成本 | 主键 `(task_id, seq)`,每个任务从 1 连续编号 |
| `contracts` | TASK_CONTRACT | 任务合约(JSON) | 每个任务一行 |
| `actions` | ACTION | 动作:工具、对象、参数、参数哈希、状态、外部回执 | 主键 `idempotency_key` |
| `approvals` | APPROVAL | 批准:绑定的参数哈希、批准人、过期时间、状态 | 按 `(task_id, status)` 索引 |
| `deliveries` | DELIVERY、VERIFICATION | 交付清单(JSON),含逐条验收结果 | 每个任务一行,重开后再交付时覆盖;历史在时间线里 |
| `handoffs` | (交接) | 交接包(JSON)和接手人 | 每个任务一行 |
| `costs` | COST_ENTRY | 成本:阶段、类别、金额(分)、人工分钟 | 按 `task_id` 索引 |
| `security_evaluations` | (研究结果) | 委员会对每只股票的结论(JSON),否决单独成列 | 主键 `(task_id, ticker)` |
| `drafts` | (研究结果) | 调仓草稿的权重 | 只有闸门能写:写入要带闸门签发的凭证 |
| `agent_calibration` | (学习) | 每个专家的可信度 | 主键 `agent_name` |
| `curated_memory` | (学习) | 结案后整理的经验,等人审核 | 主键 `memory_id` |

图 8 里的 PLAN(计划和步骤依赖)还没有表,等接入模型、出现多步计划时再加。

## 几个关键设计

- **去重**:`tasks.idempotency_trigger_key` 唯一。同一个触发事件到达两次,第二次插入撞上唯一约束,返回已有任务。
- **幂等**:`actions` 以幂等键为主键,一个动作只有一行。只有「没发出去」(`FAILED`)的动作能用同一个键重发,重发时更新这一行并计一次重试。
- **条件更新**:任务的每次更新都是 `UPDATE tasks ... WHERE task_id = ? AND version = ?`,并把版本号加一。没有更新到行就返回冲突,调用方重新读取、重新判断。闸门在请求发出之前遇到冲突会自动重来;发出之后不会。
- **一个决定一个事务**:
  - 请求批准:插入批准记录,同时任务进入 `AWAITING_APPROVAL`。
  - 发出动作:批准从 `APPROVED` 改为 `CONSUMED`(条件:任务、参数哈希都对得上,且没过期)、动作登记为 `DISPATCHED`、任务按版本号更新,三件事同一个事务,任何一件失败都不生效。
  - 批准决定:批准状态和任务的下一个状态一起写。
  - 交付、转人工、对账:各自的记录和任务状态一起写。
- **批准**:只对一个参数哈希有效,只能用一次,会过期。批准后、用掉之前还可以撤回。一个动作被证实没发出去时,它用过的批准恢复为 `APPROVED`,重发不需要再批一次。
- **结果未知**:动作发出后出错,动作记为 `UNKNOWN`、任务进入 `OUTCOME_UNKNOWN`。只要还有已发出但没定论的动作,同一个任务的其他动作都会被闸门拦下,任务也不能取消或判失败。
- **花费**:接口和工具的花费在记成本的同一个事务里加到 `tasks.spend_cents`,上限检查总能看到;人工时间只计入单位成本。
- **单位成本**:只算已经结束的任务的全部成本,除以完成的任务数;重开后再完成只算一次。
- **任务类型版本不变**:同一版本的配置写入时内容不同会报错,改规则必须升版本号。每个任务记着自己用的版本,审计时能知道当时的规则。

## 上云

1. 准备一个托管的 PostgreSQL,把连接串交给服务:`trade serve --db postgres://用户:密码@主机:5432/库名`。启动时自动建表。
2. 上线前在 PostgreSQL 上把存储层的测试跑一遍:`TRADE_TEST_PG_URL=postgres://...:5432/postgres cargo test -p trade-store`。每个测试建一个新库,需要建库权限。本机可以用 `docker compose up -d db` 起一个。
3. 连接池上限现在是 5,按实例数和数据库的连接上限调整。
4. 现在假设只有一个服务进程:启动时对账的逻辑会把所有「已发出」的动作当作上次进程留下的。要跑多个实例,先给对账加上按实例的租约。

PostgreSQL 上的测试还没有跑过:开发这一版时本机没有可用的 PostgreSQL。SQL 只用了两边都支持的写法,但在真正跑过之前不能算验证过。
