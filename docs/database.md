# 数据库设计

先用轻量的 SQLite 开发,之后换成云上的 PostgreSQL 时只改连接串。这份文档说明表怎么设计、靠什么保证可移植、上云要做哪几步。

表之间的关系见 [`diagrams.md`](diagrams.md) 的图 8。

## 可移植规则

模型代码只有一份,在两种数据库上行为一致,靠下面五条:

1. **只用可移植的列类型**:字符串、文本、定点小数、JSON、时间。不用任何数据库特有的类型。
2. **约束和索引的名字是算出来的**:在 `app/db.py` 的 `NAMING` 里定了命名规则,同一个约束在 SQLite 和 PostgreSQL 上同名,迁移脚本才能通用。
3. **JSON 列自动适配**:PostgreSQL 上是 `JSONB`,其他数据库上是普通 `JSON`。
4. **并发靠版本号,不靠数据库的锁**:任务的状态迁移用「版本号没变才更新」的条件更新,两种数据库都支持。
5. **SQLite 也开启外键检查**:SQLite 默认不检查外键,PostgreSQL 会。开启之后,本地测试就能发现插入顺序这类问题。

表结构一律通过 Alembic 迁移来改,不手工建表。

## 表

| 表 | 存什么 | 关键约束 |
|---|---|---|
| `task_type_profile` | 任务类型配置:形态、工具的风险级别、上限、放权级别 | 主键 `id` |
| `task` | 任务本身和当前状态 | `dedupe_key` 唯一;`state_version` 用于条件更新 |
| `authority` | 任务绑定的权限:角色、数据范围、凭证的引用 | 每个任务一行 |
| `task_contract` | 任务合约,带版本 | `(task_id, version)` 唯一 |
| `plan` | 计划,带版本 | `(task_id, version)` 唯一 |
| `session_event` | 任务时间线,只追加 | 按 `task_id` 索引 |
| `wait` | 等待项:等谁、等什么、何时到期、到期怎么办 | 按 `task_id` 索引 |
| `action` | 动作记录:工具、参数、参数哈希、风险级别、状态 | `idempotency_key` 唯一 |
| `approval` | 批准记录:绑定的参数哈希、证据、批准人、过期时间 | 外键指向 `action` |
| `audit_record` | 审计记录,只追加:发起人、执行身份、批准人、决定 | 按 `task_id` 索引 |
| `verification` | 验收结果:逐条标准的结论和证据 | 按 `task_id` 索引 |
| `delivery` | 交付清单 | 每个任务一行 |
| `cost_entry` | 成本台账:金额和人工分钟数,按阶段记 | 按 `task_id` 索引 |

## 几个关键设计

- **去重**:`task.dedupe_key` 唯一。同一个触发事件到达两次,第二次插入会撞上唯一约束,返回已有任务。对应蓝图里的 INTAKE。
- **幂等**:`action.idempotency_key` 唯一。同一个动作重发时用同一个键,数据库保证不会出现第二行。
- **批准绑定内容**:`approval.args_hash` 和 `action.args_hash` 必须相等。动作的参数一旦变了,原批准作废。
- **状态迁移**:只通过 `services.transition()`。它先查迁移是否合法,再做带版本号的条件更新;更新不到行就说明有并发修改,返回冲突。
- **只追加的表**:`session_event` 和 `audit_record`,应用里没有任何更新或删除它们的代码(联调用的重置除外)。
- **时间**:存不带时区的 UTC,接口输出时加 `Z`。
- **金额**:定点小数 `Numeric(12, 4)`,不用浮点。
- **主键**:应用生成的字符串,例如 `task_563e67983f874e89`,不依赖数据库的自增序列,换库和合并数据都方便。

## 迁移流程

```bash
# 改了 app/models.py 之后
.venv/bin/alembic revision --autogenerate -m "说明这次改了什么"
# 打开 migrations/versions/ 下新生成的文件,人工检查一遍
.venv/bin/alembic upgrade head
```

测试 `tests/test_migrations.py` 会从空库执行全部迁移,再和模型逐表逐列比对。改了模型却没写迁移,这个测试会失败。

## 上云步骤

1. 在云上建一个托管的 PostgreSQL 实例和数据库。
2. 安装驱动:`pip install -e ".[postgres]"`。
3. 把 `AITRADER_DATABASE_URL` 设成 `postgresql+psycopg://用户:密码@主机:5432/库名`。密码放在云平台的密钥管理里,不写进仓库。
4. 执行 `alembic upgrade head` 建表。建议把 `AITRADER_AUTO_MIGRATE` 关掉,迁移作为部署流程里的独立一步。
5. 关掉 `AITRADER_ENABLE_DEV_ENDPOINTS` 和 `AITRADER_SEED_ON_START`。

本地 SQLite 里的数据是演示数据,不需要搬。如果将来要搬真实数据,按表的外键顺序导出再导入即可,主键是字符串,不会冲突。

## 已经验证和还没验证的

已经验证(2026-11-22,本机):

- 全部测试在开启外键检查的 SQLite 上通过。
- 在 Docker 里的 PostgreSQL 16 上,从空库执行迁移成功,载入演示数据成功,15 个 JSON 列都是 `JSONB`,唯一约束按命名规则生成。

还没验证:

- 没有在真实的云数据库上跑过。
- 没有做并发压力测试。条件更新的逻辑有单元测试覆盖合法性,但没有模拟两个写入者同时竞争。

## 上云时要补的

- **审计表的权限隔离**:蓝图要求 Agent 无权写审计记录。现在所有表用同一个数据库账号,上云时给审计表单独的写入账号。
- **备份和保留期**:会话日志可以定期清理,审计记录要长期保留。
- **连接池**:多个后端进程时,前面加连接池。
- **事件流**:现在的发布订阅在进程内,多进程时换成 PostgreSQL 的 `LISTEN/NOTIFY` 或 Redis。
