# 接口指南(给前端联调)

Rust 后端的 HTTP 接口,代码在 `crates/trade-server/src/api.rs`。Agent 的每一步都是真实的用例:研究、闸门、批准、下单、对账、验收都会真的发生。联调时用 `/v1/dev` 下的接口载入演示任务、拨快时钟、给模拟券商注入故障,不需要手工改状态。

启动:

```bash
cargo run -p trade-server --bin trade -- serve --dev --db trade.db
```

## 约定

| 项 | 约定 |
|---|---|
| 地址 | `http://127.0.0.1:8000`,用 `--addr` 改 |
| 身份 | 请求头 `X-User-Id: 用户名`,开发期的占位,接入正式登录后会换掉。演示数据里发起人是 `alice`,批准人是 `bob`,配置管理员是 `platform_admin` |
| 格式 | 请求和响应都是 JSON |
| 时间 | UTC,ISO 8601,带 `Z`,例如 `2026-11-27T09:07:10Z` |
| 金额 | 请求里用美元(`portfolio_capital_usd`),`usage.spend_usd` 也是美元;合约和交付清单里原样给出分(`portfolio_capital`、`total_spend`) |
| 权重 | 基点,`10000` 是 100% |
| 分页 | 列表接口用 `limit`(1 到 200,默认 50)和 `offset`,返回 `{items, total}` |
| 跨域 | 默认只允许 `http://localhost:3000`,用环境变量 `AITRADER_CORS_ORIGINS`(逗号分隔)改 |

不需要 `X-User-Id` 的接口:`/health`、`/v1/meta/states`、`/v1/task-types`、`/v1/stream`、`/v1/dev/*`。其余都需要。

## 错误

统一是这个形状,前端按 `code` 判断,`message` 是给开发者看的英文说明:

```json
{"error": {"code": "self_approval_forbidden", "message": "refused: separation_of_duties:alice"}}
```

| HTTP | `code` | 什么时候出现 |
|---|---|---|
| 401 | `missing_identity` | 没带 `X-User-Id` |
| 403 | `self_approval_forbidden` | 发起人或配置管理员想批准 |
| 403 | `not_requester` | 不是发起人却去回答问题 |
| 403 | `not_allowed` | 不是发起人却去验收、取消、重开;不是接手人却去交回 |
| 404 | `task_not_found` `approval_not_found` `contract_not_found` | 对象不存在 |
| 409 | `invalid_transition` | 当前状态不允许这个操作 |
| 409 | `state_conflict` | 任务被别人同时改了,重新取详情后再试 |
| 409 | `unresolved_actions` | 还有已发出但结果未定的动作,不能取消或判失败 |
| 409 | `not_awaiting_input` `not_settling` | 任务不在对应的等待状态 |
| 409 | `approval_not_pending` | 这条批准已经处理过 |
| 409 | `approval_expired` | 批准已过期,按否决处理,任务同时变化(见「批准」) |
| 422 | `invalid_request` | 请求体不合法,或字段取值不认识 |
| 502 | `model_unreachable` `model_error` `model_bad_response` `external_error` | 模型或行情数据出错;任务保持原样,可以再推进 |

闸门拒绝一个动作不是接口错误,而是 `advance` 的结果之一(`step.kind = "denied"`,见下文)。

## 任务状态

`GET /v1/meta/states` 返回这张表,前端用它渲染看板的列和标签,不要把状态名写死。每个任务还带 `allowed_next_states`,由图 4a 的迁移表算出。

| `state` | `state_label` | `owner_role`:此刻该谁行动 | 前端该给的操作 |
|---|---|---|---|
| `QUEUED` | 排队 | operator | 取消 |
| `RUNNING` | 运行中 | operator | 推进、取消 |
| `AWAITING_INPUT` | 等回答 | requester | 发起人回答 |
| `AWAITING_APPROVAL` | 等批准 | approver | 批准人批准或否决 |
| `OUTCOME_UNKNOWN` | 结果未知 | reconciler | 推进(会先对账)。不能取消 |
| `LIMIT_REACHED` | 触上限 | owner | 推进(验收已完成的部分) |
| `VERIFYING` | 验收中 | verifier | 只读 |
| `SETTLING` | 待确认 | requester | 发起人验收或退回 |
| `COMPLETED` | 已完成 | none | 发起人重开 |
| `FAILED` | 已失败 | owner | 只读 |
| `HANDED_OFF` | 转人工 | assignee | 接手人交回 |
| `DECLINED` | 被否决 | requester | 只读 |
| `CANCELLED` | 已取消 | none | 只读 |

## 接口一览

| 方法和路径 | 用途 | 谁能调 |
|---|---|---|
| `GET /health` | 健康检查,带数据库类型和当前模型(没有为 `null`) | 任何人 |
| `GET /v1/meta/states` | 状态表 | 任何人 |
| `GET /v1/task-types` | 当前任务类型配置:工具风险分级、上限、放权级别、等待期限 | 任何人 |
| `POST /v1/tasks` | 创建任务并定义合约,自动去重 | 已识别的用户,即发起人 |
| `GET /v1/tasks` | 任务列表,可按 `state`、`requester` 过滤 | 已识别的用户 |
| `GET /v1/tasks/{id}` | 任务详情 | 已识别的用户 |
| `GET /v1/tasks/{id}/events` | 时间线(审计记录) | 已识别的用户 |
| `GET /v1/tasks/{id}/actions` | 动作记录 | 已识别的用户 |
| `GET /v1/tasks/{id}/evaluations` | 委员会对每只股票的结论和专家证据 | 已识别的用户 |
| `POST /v1/tasks/{id}/advance` | 让 Agent 往下走,直到需要人 | 已识别的用户 |
| `POST /v1/tasks/{id}/answer` | 回答澄清问题 | 发起人 |
| `POST /v1/tasks/{id}/settle` | 验收或退回 | 发起人 |
| `POST /v1/tasks/{id}/cancel` | 取消 | 发起人 |
| `POST /v1/tasks/{id}/reopen` | 重开已完成的任务 | 发起人 |
| `POST /v1/tasks/{id}/hand-back` | 转人工后交回 Agent | 接手人 |
| `GET /v1/approvals` | 批准收件箱,可按 `status` 过滤 | 已识别的用户 |
| `GET /v1/approvals/{id}` | 批准详情:确切内容和规则 | 已识别的用户 |
| `POST /v1/approvals/{id}/decision` | 批准或否决 | 发起人和配置管理员以外的人 |
| `GET /v1/metrics/scorecard` | 记分卡 | 已识别的用户 |
| `GET /v1/stream` | 实时事件流 | 任何人 |
| `POST /v1/dev/seed` | 载入演示任务 | 联调 |
| `POST /v1/dev/clock` | 拨快时钟,处理到期的等待 | 联调 |
| `POST /v1/dev/broker-fault` | 让模拟券商的下一笔单出故障 | 联调 |

## 创建任务

`POST /v1/tasks`:

```json
{
  "goal": "2026-02-10 的价值与质量调仓",
  "as_of_date": "2026-02-10",
  "portfolio_capital_usd": 1000000,
  "max_positions": 3,
  "orders_optional": true
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `goal` | 是 | 任务目标,写进合约 |
| `as_of_date` | 否 | 调仓日。缺了或格式不对,任务会停在 `AWAITING_INPUT` 问发起人 |
| `portfolio_capital_usd` | 否 | 资金。缺了同上 |
| `markets` | 否 | 研究范围,默认 `["US"]`。超出发起人被授权的市场时同样会问 |
| `max_positions` | 否 | 最多几只,默认 3 |
| `orders_optional` | 否 | 下单的批准被否决时,能不能只交付草稿。默认 `true`;为 `false` 时否决下单会让任务以 `DECLINED` 结束 |
| `trigger_key` | 否 | 触发事件自带的标识,用来去重。不传时由发起人和请求内容算出 |

新建返回 201,去重命中返回 200 且 `created` 为 `false`,`task` 是已有的那个任务:

```json
{"created": true, "task": {"id": "TASK-40f4d4b068", "state": "RUNNING", "...": "..."}}
```

## 任务详情

列表里的每个任务:

```json
{
  "id": "TASK-40f4d4b068",
  "task_type": "value_committee_rebalance",
  "config_version": "3.2.0",
  "state": "AWAITING_APPROVAL",
  "state_label": "等批准",
  "owner_role": "approver",
  "state_detail": null,
  "state_version": 6,
  "requester": "alice",
  "acting_principal": "quant_research_svc",
  "markets": ["US"],
  "wait": {"owner": "portfolio_approver", "deadline": "2026-11-28T09:07:10Z", "on_timeout": "DECLINE"},
  "usage": {"turns": 22, "spend_usd": 0.21, "retries": 0, "consecutive_denials": 0, "reworks": 0, "reopened": 0},
  "allowed_next_states": ["RUNNING", "HANDED_OFF", "DECLINED", "CANCELLED"]
}
```

- `wait` 只在等待状态出现:等谁(`owner`)、到什么时候(`deadline`)、到期怎么办(`on_timeout`)。到期动作有四种:`HAND_OFF`(转人工)、`DECLINE`(按否决处理)、`COMPLETE`(按验收通过结案)、`ESCALATE`(交给下一个负责人)。到期永远不会当成批准。
- `state_detail` 是 `LIMIT_REACHED`、`FAILED`、`DECLINED`、`CANCELLED` 的原因,或 `OUTCOME_UNKNOWN` 冻结的动作的幂等键。
- `usage` 对应任务类型配置里的上限。

`GET /v1/tasks/{id}` 另外带:

| 字段 | 内容 |
|---|---|
| `open_questions` | `AWAITING_INPUT` 时要发起人回答的问题,例如 `["missing_as_of_date"]` |
| `contract` | 合约:目标、调仓日、范围、资金(分)、上限、验收标准、假设 |
| `plan` | 计划(图 2):`version` 和 `steps`,每一步有 `id`、`tool`、`depends_on`、`skippable` 和 `status`(`done`、`ready`、`blocked`、`skipped`)。有人否决可以跳过的步骤后,`version` 加一、那一步变成 `skipped` |
| `approvals` | 这个任务的批准,格式同批准收件箱 |
| `draft` | 最新的调仓草稿:`draft_id`、`as_of_date`、`weights`(基点) |
| `delivery` | 交付清单:`completeness`(`COMPLETE`、`PARTIAL`)、逐条验收结果 `criteria_results`、缺口 `gaps`、权重、花费 |
| `handoff` | 交接包:原因、已完成和未完成的动作、已有权重、待回答的问题、接手人 |

## 推进

`POST /v1/tasks/{id}/advance` 让 Agent 一直做下去,直到需要人:研究、写草稿、下单、验收、交付。可以随时、重复调用,每一步都从数据库重新读。

服务配置了模型时(`serve --model-url ... --model ...`),每一轮由模型提议一个工具调用:研究、看筛选结果、读公告、写草稿(权重由模型定)、下单、完成。写操作先过闸门,结果、拒绝原因和验收缺口作为数据告诉模型,它据此修改提议。模型的每一句和得到的每个结果都写进时间线,`kind` 分别是 `agent.reply` 和 `agent.observation`。需要批准时同样停下;批准之后按批准的确切内容发出,不再问模型。没配置模型时按固定规则推进。一次推进可能要等模型几分钟,客户端的超时要留够。

返回 `{step, task}`,`step.kind` 有五种:

| `step.kind` | 意思 | 前端接着做什么 |
|---|---|---|
| `needs_approval` | 一个动作等人批准,带 `approval_id` 和 `tool` | 去批准收件箱 |
| `denied` | 闸门拒绝了这个动作,`reason` 是原因,例如 `business_rule_violation:vetoed_ticker_in_portfolio:TRAP_CO` | 展示原因。连续被拒到上限后任务会停下 |
| `delivered` | 已验收并交付,任务进入 `SETTLING`,带 `completeness` 和 `gaps` | 请发起人验收 |
| `handed_off` | 转人工了,`owner` 是接手人 | 展示交接包 |
| `waiting` | 任务在等人或已结束,`state` 是当前状态 | 无 |

验收只看库里的事实:专家引用的记录在调仓日之前已公开,草稿里没有被否决的股票,权重满足总量、单只、行业和流动性上限,每个动作都已核对并且券商那边还能读到一样的单。没通过会自动返工,返工次数用完就转人工。

## 回答、验收、取消、重开、交回

| 接口 | 请求体 | 成功后 |
|---|---|---|
| `POST /v1/tasks/{id}/answer` | 和创建任务一样的字段,补全缺的那项 | `RUNNING`,或者还缺东西时仍是 `AWAITING_INPUT`。返回 `{open_questions, task}` |
| `POST /v1/tasks/{id}/settle` | `{"decision": "accept", "review_minutes": 3}` | `COMPLETED`。`review_minutes` 计入人工成本 |
| `POST /v1/tasks/{id}/settle` | `{"decision": "return", "comment": "补上缺口"}` | `RUNNING`,算一次返工 |
| `POST /v1/tasks/{id}/cancel` | `{"reason": "不需要了"}` | `CANCELLED` |
| `POST /v1/tasks/{id}/reopen` | `{"reason": "发现公司行动遗漏"}` | `RUNNING`,算一次返工 |
| `POST /v1/tasks/{id}/hand-back` | 无 | `RUNNING` |

都返回更新后的任务详情(`answer` 除外,见上)。发起人在验收窗口内既不验收也不退回,窗口结束时按验收通过结案。

## 批准

`GET /v1/approvals?status=PENDING` 是收件箱,`status` 可以是 `PENDING`、`APPROVED`、`REJECTED`、`CONSUMED`、`EXPIRED`。每条:

```json
{
  "id": "APR-5d74dcf00084",
  "task_id": "TASK-5de74f48b8",
  "tool": "submit_paper_orders",
  "target": "paper_broker:2026-02-10",
  "tier": "R3",
  "rule": "R3: consequential and not reversible; a person approves this exact payload, once",
  "payload": {"draft_id": "DRAFT-TASK-5de74f48b8", "as_of_date": "2026-02-10", "weights_bps": {"AAPL_MOAT": 2681, "CAT_IND": 3500, "MSFT_QUAL": 3144}, "markets": ["US"], "portfolio_capital_cents": 100000000},
  "payload_hash": "b0a97a2791c14219...",
  "status": "PENDING",
  "requester": "pm_alice",
  "approver": null,
  "note": null,
  "expires_at": "2026-11-28T09:07:10Z"
}
```

界面上把 `payload`(批准的确切内容)和 `rule` 放在最显眼的位置。批准只对这个 `payload_hash` 有效、只能用一次。

`POST /v1/approvals/{id}/decision`,请求体 `{"decision": "approve", "reason": "在额度内"}` 或 `{"decision": "reject", "reason": "超出额度"}`。返回更新后的批准。

- 发起人和配置管理员不能批准,返回 403 `self_approval_forbidden`。
- 批准后任务回到 `RUNNING`,再推进时闸门核对并用掉这条批准。
- 否决草稿(R2)时任务以 `DECLINED` 结束。否决下单(R3)时,合约允许(`orders_optional`)就回到 `RUNNING`,只交付草稿,交付清单为 `PARTIAL`;不允许就以 `DECLINED` 结束。
- 过期的批准不能再批,返回 409 `approval_expired`,同时按否决处理。没人处理的批准到期时,后台也会这样处理。

## 记分卡

`GET /v1/metrics/scorecard`:

```json
{
  "tasks_by_state": {"AWAITING_APPROVAL": 2, "COMPLETED": 1, "SETTLING": 1},
  "approvals_by_status": {"CONSUMED": 5, "PENDING": 2, "REJECTED": 1},
  "unit_economics": {
    "ended_tasks": 3,
    "completed_tasks": 1,
    "api_and_tool_usd": 0.44,
    "human_minutes": 2.0,
    "all_in_cost_usd": 4.44,
    "cost_per_completed_task_usd": 4.44
  },
  "human_hourly_rate_usd": 120.0
}
```

单位成本只算已经结束的任务(完成、失败、被否决、取消、转人工):它们的全部成本(接口和工具花费,加上人工时间按小时费率折算)除以完成的任务数。重开后再完成的任务只算一次,进行中的任务不计入。没有完成的任务时为 `null`。

## 实时事件

`GET /v1/stream` 是 Server-Sent Events。任务被创建或发生变化时推一条,包括后台处理到期等待造成的变化;每 15 秒一条心跳注释。

```js
const es = new EventSource("http://127.0.0.1:8000/v1/stream");
es.addEventListener("task.created", (e) => { /* 列表失效重取 */ });
es.addEventListener("task.updated", (e) => {
  const { task_id, state, state_version } = JSON.parse(e.data);
  // 重新取这个任务的详情
});
```

事件只带 `{type, task_id, state, state_version}`。客户端跟不上时会漏掉一些,收到下一条时重取即可。

## 联调接口

`/v1/dev/*` 只在 `serve --dev` 或环境变量 `AITRADER_ENABLE_DEV_ENDPOINTS=true` 时存在,上线不开。

`POST /v1/dev/seed` 用真实的用例造出八个演示任务,发起人都是 `alice`。重复调用不会重复创建。

| 名字 | 状态 | 怎么来的 |
|---|---|---|
| `queued` | `QUEUED` | 只受理 |
| `needs-input` | `AWAITING_INPUT` | 请求里没写调仓日 |
| `draft-approval` | `AWAITING_APPROVAL` | 研究完,草稿等批准 |
| `order-approval` | `AWAITING_APPROVAL` | `bob` 批了草稿,下单等批准 |
| `settling` | `SETTLING` | 两次都批了,已交付 |
| `completed` | `COMPLETED` | 再由 `alice` 验收 |
| `declined` | `DECLINED` | `bob` 否决了草稿 |
| `handed-off` | `HANDED_OFF` | 直接转人工 |

`POST /v1/dev/clock`,请求体 `{"advance_s": 86401}`:把时钟拨快,然后处理所有到期的等待,返回 `{now, handled}`。可以用来看批准过期、验收窗口结束、问题没人回答时转人工。

`POST /v1/dev/broker-fault`,请求体 `{"fault": "timeout_after_fill"}` 或 `{"fault": "drop_before_fill"}`:模拟券商的下一笔单超时。前者是单已成交但回执丢了,后者是请求没到券商。推进时任务先进入 `OUTCOME_UNKNOWN`,再按券商那边的记录对账:成交了就记为已完成,没到就重发一次,不会下第二笔单。

## 联调脚本

一个任务从创建走到完成,经过两次批准和一次验收。

```bash
B=http://127.0.0.1:8000
J='Content-Type: application/json'

# 1. 创建,记下返回的 task.id
curl -s -X POST -H "X-User-Id: alice" -H "$J" $B/v1/tasks \
  -d '{"goal":"调仓","as_of_date":"2026-02-10","portfolio_capital_usd":1000000}'
TID=<task.id>

# 2. 推进:研究完,草稿等批准。记下 step.approval_id
curl -s -X POST -H "X-User-Id: ops" $B/v1/tasks/$TID/advance
AID=<step.approval_id>

# 3. 发起人自己批会被拒(403),换 bob 批
curl -s -X POST -H "X-User-Id: alice" -H "$J" -d '{"decision":"approve"}' $B/v1/approvals/$AID/decision
curl -s -X POST -H "X-User-Id: bob" -H "$J" -d '{"decision":"approve"}' $B/v1/approvals/$AID/decision

# 4. 再推进:写草稿,下单等批准。bob 批完再推进:下单、核对、验收、交付
curl -s -X POST -H "X-User-Id: ops" $B/v1/tasks/$TID/advance
curl -s -X POST -H "X-User-Id: bob" -H "$J" -d '{"decision":"approve"}' $B/v1/approvals/<新的 approval_id>/decision
curl -s -X POST -H "X-User-Id: ops" $B/v1/tasks/$TID/advance

# 5. 发起人验收,然后看时间线和记分卡
curl -s -X POST -H "X-User-Id: alice" -H "$J" -d '{"decision":"accept","review_minutes":3}' $B/v1/tasks/$TID/settle
curl -s -H "X-User-Id: alice" $B/v1/tasks/$TID/events
curl -s -H "X-User-Id: alice" $B/v1/metrics/scorecard
```

同样的流程在 `crates/trade-server/tests/api.rs` 里有自动化版本。

## 现在还没有的

前端先不要依赖这些:

- 真实登录和角色。现在 `X-User-Id` 写什么就是谁;批准只校验不是发起人和配置管理员,不校验批准人角色。
- 机器可读的接口定义(OpenAPI)。原来的 `openapi.json` 是 Python 版生成的,已删除。
- 加额度(`LIMIT_REACHED` 之后继续做)的接口。现在只能验收已完成的部分。
- 任务类型配置的增删改。现在从 `config/task_types/` 读,改了要重启并升版本号。
- 事件流的身份校验和断线续传。
- 用模型当专家。现在模型只做主 Agent 的提议,三个专家还是确定性的规则。
- 后台自动推进。现在 `advance` 由前端或脚本调用。
