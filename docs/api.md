# 接口指南(给前端联调)

后端骨架的 HTTP 接口。Agent 还没接入,任务靠 `/v1/dev` 下的联调接口推进,所以前端现在就能把每个状态、每个页面都跑通。

- 交互式文档:后端启动后打开 `http://localhost:8000/docs`,可以直接在页面上发请求。
- 机器可读的定义:[`openapi.json`](openapi.json)。可以用它生成前端类型,例如 `npx openapi-typescript docs/openapi.json -o frontend/src/api/schema.d.ts`。
- 启动后端:见 [`backend/README.md`](../backend/README.md)。

## 约定

| 项 | 约定 |
|---|---|
| 地址 | 开发环境 `http://localhost:8000` |
| 身份 | 请求头 `X-User-Id: 用户名`。这是开发期的占位,正式登录接入后会换掉。演示数据里发起人是 `alice`,批准人用 `bob` |
| 格式 | 请求和响应都是 JSON |
| 时间 | UTC,ISO 8601,带 `Z`,例如 `2026-11-22T09:07:10.113032Z` |
| 分页 | 列表接口用 `limit`(1 到 200,默认 50)和 `offset`,返回 `{items, total}` |
| 跨域 | 默认只允许 `http://localhost:3000`,改 `AITRADER_CORS_ORIGINS` |

不需要 `X-User-Id` 的接口:`/health`、`/v1/meta/states`、`/v1/profiles`、`/v1/stream`、`/v1/dev/*`。其余都需要。

## 错误

业务错误统一是这个形状,前端按 `code` 判断,`message` 是给开发者看的英文说明:

```json
{
  "error": {
    "code": "self_approval_forbidden",
    "message": "the requester cannot approve their own task"
  }
}
```

| HTTP | `code` | 什么时候出现 |
|---|---|---|
| 401 | `missing_identity` | 没带 `X-User-Id` |
| 403 | `self_approval_forbidden` | 发起人想批准自己的任务 |
| 403 | `not_requester` | 不是发起人却去回答澄清问题 |
| 403 | `not_allowed` | 不是发起人或负责人却去取消任务 |
| 404 | `task_not_found` `profile_not_found` `approval_not_found` | 对象不存在 |
| 409 | `invalid_transition` | 当前状态不允许这个操作 |
| 409 | `state_conflict` | 任务被别人同时改了,重新取详情后再试 |
| 409 | `unresolved_actions` | 还有结果未知的动作,不能判失败 |
| 409 | `not_awaiting_input` `not_settling` | 任务不在对应的等待状态 |
| 409 | `approval_not_pending` | 这条批准已经处理过 |
| 409 | `approval_expired` | 批准已过期。任务同时被置为 `DECLINED` |
| 409 | `payload_changed` | 动作内容在请求批准之后变了,原批准作废 |
| 422 | (无 `code`) | 请求体不合法。这是框架默认格式 `{"detail": [...]}` |

## 任务状态

`GET /v1/meta/states` 返回这张表,前端用它渲染看板的列和状态标签,不要把状态名写死在前端。

| `state` | `state_label` | `owner_role`:此刻该谁行动 | 前端该给的操作 |
|---|---|---|---|
| `QUEUED` | 排队 | operator | 取消 |
| `RUNNING` | 运行中 | operator | 取消 |
| `AWAITING_INPUT` | 等回答 | requester | 发起人回答问题 |
| `AWAITING_APPROVAL` | 等批准 | approver | 批准人批准或否决 |
| `OUTCOME_UNKNOWN` | 结果未知 | reconciler | 只读。不能取消 |
| `LIMIT_REACHED` | 触上限 | owner | 只读(加额度的接口还没做) |
| `VERIFYING` | 验收中 | verifier | 只读 |
| `SETTLING` | 待确认 | reviewer | 验收或退回 |
| `COMPLETED` | 已完成 | 无 | 只读 |
| `FAILED` | 已失败 | owner | 只读 |
| `HANDED_OFF` | 转人工 | assignee | 只读 |
| `DECLINED` | 被否决 | requester | 只读 |
| `CANCELLED` | 已取消 | 无 | 只读 |

任务详情里的 `allowed_next_states` 列出当前状态能去哪些状态。完整的迁移规则见 [`diagrams.md`](diagrams.md) 的图 4a。

## 接口一览

| 方法和路径 | 用途 | 用在哪个页面 |
|---|---|---|
| `GET /health` | 健康检查 | |
| `GET /v1/meta/states` | 状态表 | 看板 |
| `GET /v1/profiles` | 任务类型配置 | 新建任务、配置页 |
| `POST /v1/tasks` | 创建任务,自动去重 | 新建任务 |
| `GET /v1/tasks` | 任务列表,可按 `state`、`requester` 过滤 | 看板 |
| `GET /v1/tasks/{task_id}` | 任务详情:合约、计划、等待项、交付清单 | 任务详情 |
| `GET /v1/tasks/{task_id}/events` | 时间线 | 任务详情 |
| `GET /v1/tasks/{task_id}/actions` | 动作记录 | 任务详情 |
| `POST /v1/tasks/{task_id}/answer` | 回答澄清问题 | 任务详情 |
| `POST /v1/tasks/{task_id}/settle` | 验收或退回 | 交付与验收 |
| `POST /v1/tasks/{task_id}/cancel` | 取消 | 任务详情 |
| `GET /v1/approvals` | 批准列表,可按 `status` 过滤 | 批准收件箱 |
| `GET /v1/approvals/{approval_id}` | 批准详情和证据 | 批准收件箱 |
| `POST /v1/approvals/{approval_id}/decision` | 批准或否决 | 批准收件箱 |
| `GET /v1/metrics/scorecard` | 记分卡 | 记分卡 |
| `GET /v1/stream` | 实时事件流 | 所有页面 |
| `POST /v1/dev/seed` | 重置并载入演示数据 | 联调 |
| `POST /v1/dev/tasks/{task_id}/simulate` | 模拟 Agent 的一步 | 联调 |

每个接口的完整字段见 `/docs` 页面或 `openapi.json`。下面是最常用的几个。

### 创建任务

`POST /v1/tasks`,请求体:

```json
{
  "profile_id": "research_paper_order",
  "title": "研究 ACME 第三季度财报并给出要点",
  "request_text": "只用公司公告和官方披露。"
}
```

新建成功返回 201。去重命中返回 200,`created` 为 `false`,`task` 是已有的那个任务。可选字段 `dedupe_key` 用来传触发事件自带的标识;不传时由任务类型、发起人、标题和请求内容算出。

```json
{
  "task": {
    "id": "task_c8e9fb2609ae44e6",
    "profile_id": "research_paper_order",
    "title": "研究 ACME 第三季度财报并给出要点",
    "state": "QUEUED",
    "state_label": "排队",
    "owner_role": "operator",
    "requester": "alice",
    "owner": "alice",
    "outcome_class": null,
    "due_at": null,
    "created_at": "2026-11-22T09:07:10.113032Z",
    "updated_at": "2026-11-22T09:07:10.113032Z"
  },
  "created": true
}
```

### 任务详情

`GET /v1/tasks/{task_id}`。除了列表里的字段,还带合约、计划、未解决的等待项和交付清单。下面是一个等回答的任务(`plan.steps` 和 `success_criteria` 各只留了一条):

```json
{
  "id": "task_8633c3e74b004af2",
  "title": "核对 ACME 与同业的毛利率差异",
  "state": "AWAITING_INPUT",
  "state_label": "等回答",
  "owner_role": "requester",
  "requester": "alice",
  "state_version": 3,
  "allowed_next_states": ["CANCELLED", "HANDED_OFF", "RUNNING"],
  "contract": {
    "version": 1,
    "goal": "核对 ACME 与同业的毛利率差异",
    "success_criteria": [{"text": "每个数字都引用官方披露", "check": "citation_audit"}],
    "assumptions": ["币种按披露原币种"]
  },
  "plan": {
    "version": 1,
    "steps": [{"id": "s1", "title": "检索官方披露", "reversible": true, "status": "pending"}]
  },
  "open_wait": {
    "id": 1,
    "kind": "INPUT",
    "addressee": "alice",
    "question": {
      "text": "资料范围只用官方披露,还是可以包含媒体报道?",
      "options": ["只用官方披露", "可以包含媒体报道"],
      "default": "只用官方披露"
    },
    "expires_at": "2026-11-24T09:07:10.088163Z",
    "on_expiry": "hand_off"
  },
  "delivery": null
}
```

`open_wait.kind` 有三种:`INPUT`(等发起人回答)、`APPROVAL`(等批准,`question.approval_id` 指向批准记录)、`REVIEW`(等验收)。

### 回答、验收、取消

| 接口 | 请求体 | 谁能调 | 成功后的状态 |
|---|---|---|---|
| `POST /v1/tasks/{id}/answer` | `{"answer": {"choice": "只用官方披露"}}` | 只有发起人 | `RUNNING` |
| `POST /v1/tasks/{id}/settle` | `{"decision": "accept", "comment": ""}` | 任何已识别的用户 | `COMPLETED` |
| `POST /v1/tasks/{id}/settle` | `{"decision": "return", "comment": "补上缺口"}` | 任何已识别的用户 | `RUNNING` |
| `POST /v1/tasks/{id}/cancel` | `{"reason": ""}` | 发起人或负责人 | `CANCELLED` |

三个接口都返回更新后的任务详情。

### 批准

`GET /v1/approvals?status=PENDING` 是批准收件箱。每条批准带一份由代码整理的证据 `evidence`:

```json
{
  "id": "apr_a5b4127066ff472c",
  "op_id": "op_c5cf516395cf4e53",
  "task_id": "task_f2da1457a2dd4536",
  "args_hash": "53556f07849906077393a98bad38186f9003853dee521928db3c7256db26b287",
  "evidence": {
    "payload": {"symbol": "ACME", "side": "buy", "quantity": 100, "order_type": "limit", "limit_price": "12.50", "venue": "paper"},
    "rule": "create_order_request 属于 R3,必须人工批准",
    "estimated_notional": "1250.00",
    "position_before": 0,
    "position_after": 100,
    "basis": ["ACME 第三季度公告", "研究报告草稿 v1"],
    "model_note": {"text": "限价低于最近成交价", "trusted": false}
  },
  "required_role": "approver",
  "status": "PENDING",
  "approver": null,
  "reason": "",
  "created_at": "2026-11-22T09:07:10.090548Z",
  "expires_at": "2026-11-23T09:07:10.090548Z",
  "decided_at": null
}
```

界面上要把 `evidence.payload`(确切内容)和 `evidence.rule` 放在最显眼的位置。`model_note` 是模型自己的说法,`trusted` 为 `false`,要和事实分开显示。

`POST /v1/approvals/{approval_id}/decision`,请求体 `{"decision": "approve", "reason": "金额在额度内"}` 或 `{"decision": "reject", "reason": "超出额度"}`。

- 批准人不能是任务的发起人,否则 403 `self_approval_forbidden`。
- 批准后任务回到 `RUNNING`,动作状态变为 `APPROVED`。
- 否决后任务变为 `DECLINED`。等 Agent 接入后,能绕开被否决动作的任务会回到 `RUNNING`,这一点以后会变。
- 过期的批准不能再批,返回 409 `approval_expired`,任务变为 `DECLINED`。

### 记分卡

`GET /v1/metrics/scorecard`:

```json
{
  "tasks_by_state": {"RUNNING": 2, "COMPLETED": 1, "SETTLING": 1},
  "total_tasks": 8,
  "completed": 1,
  "closed": 1,
  "total_cost": 0.358,
  "human_minutes": 11,
  "cost_per_successful_task": 0.358,
  "approvals_by_status": {"APPROVED": 1, "PENDING": 1}
}
```

`cost_per_successful_task` 是全部任务的成本除以已完成的任务数,没有已完成任务时为 `null`。`tasks_by_state` 只包含数量不为零的状态。

## 实时事件

`GET /v1/stream` 是 Server-Sent Events。任务被创建或状态变化时推一条,每 15 秒有一条心跳注释。

```js
const es = new EventSource("http://localhost:8000/v1/stream");
es.addEventListener("task.updated", (e) => {
  const { task_id, state, state_version } = JSON.parse(e.data);
  // 重新取这个任务的详情,或者让列表失效重取
});
es.addEventListener("task.created", (e) => { /* 同上 */ });
```

事件只带 `{type, task_id, state, state_version}`,不带完整任务。收到后重新请求详情。

## 联调接口

`/v1/dev/*` 只在 `AITRADER_ENABLE_DEV_ENDPOINTS=true` 时存在,上线会关掉。

### 载入演示数据

`POST /v1/dev/seed` 清空所有表,载入一个任务类型 `research_paper_order` 和八个任务。公司名和数字都是虚构的。

| 任务标题 | 状态 |
|---|---|
| ACME 管理层变动梳理 | `QUEUED` |
| 研究 ACME 第三季度财报并给出要点 | `RUNNING` |
| 核对 ACME 与同业的毛利率差异 | `AWAITING_INPUT` |
| ACME 模拟盘买入 100 股,限价 12.50 | `AWAITING_APPROVAL` |
| ACME 模拟盘卖出 50 股 | `OUTCOME_UNKNOWN` |
| 汇总 ACME 近三年现金流 | `LIMIT_REACHED` |
| 整理 ACME 风险因素清单 | `SETTLING` |
| ACME 估值区间研究 | `COMPLETED` |

### 模拟 Agent 的一步

`POST /v1/dev/tasks/{task_id}/simulate`,请求体 `{"step": "start"}`。返回更新后的任务详情。当前状态不允许这一步时返回 409 `invalid_transition`。

| `step` | 要求的当前状态 | 之后的状态 | 同时生成的记录 |
|---|---|---|---|
| `start` | `QUEUED` | `RUNNING` | 合约和计划(没有时)、一条模型成本 |
| `ask` | `RUNNING` | `AWAITING_INPUT` | 一个 `INPUT` 等待项,带问题和选项 |
| `request_approval` | `RUNNING` | `AWAITING_APPROVAL` | 一条 R3 动作、一条待批准记录、一个 `APPROVAL` 等待项、一条审计记录 |
| `unknown_outcome` | `RUNNING` | `OUTCOME_UNKNOWN` | 把已批准的动作标为 `UNKNOWN`,没有就新建一条 |
| `resolve_unknown` | `OUTCOME_UNKNOWN` | `RUNNING` | `UNKNOWN` 的动作变为 `CONFIRMED`,带外部回执 |
| `limit` | `RUNNING` | `LIMIT_REACHED` | 无 |
| `extend_limit` | `LIMIT_REACHED` | `RUNNING` | 无 |
| `claim_done` | `RUNNING` 或 `LIMIT_REACHED` | `VERIFYING` | 无 |
| `rework` | `VERIFYING` | `RUNNING` | 无 |
| `deliver` | `VERIFYING` | `SETTLING` | 逐条验收结果、交付清单、一个 `REVIEW` 等待项、一条评分成本 |
| `fail` | `QUEUED`、`RUNNING` 或 `VERIFYING` | `FAILED` | 无。有结果未知的动作时会被拒绝 |
| `hand_off` | 任何未结束的状态 | `HANDED_OFF` | 一条交接包事件 |

`deliver` 可以加 `"partial": true`,交付清单的 `outcome_class` 变为 `partial`,最后一条标准标为未达成。

## 联调脚本

把一个任务从排队走到完成,覆盖批准和验收两个人工环节。先设好地址:

```bash
B=http://localhost:8000
```

1. 重置数据,取一个排队中的任务:

```bash
curl -s -X POST $B/v1/dev/seed
curl -s -H "X-User-Id: alice" "$B/v1/tasks?state=QUEUED"
```

2. 把上一步返回的任务 `id` 记为 `TID`,模拟 Agent 开始并请求批准:

```bash
curl -s -X POST -H "Content-Type: application/json" -d '{"step":"start"}' $B/v1/dev/tasks/$TID/simulate
curl -s -X POST -H "Content-Type: application/json" -d '{"step":"request_approval"}' $B/v1/dev/tasks/$TID/simulate
```

3. 批准人查看收件箱。把这个任务对应的批准 `id` 记为 `AID`:

```bash
curl -s -H "X-User-Id: bob" "$B/v1/approvals?status=PENDING"
```

4. 发起人自己批准会被拒绝(403),换批准人批准:

```bash
curl -s -X POST -H "X-User-Id: alice" -H "Content-Type: application/json" -d '{"decision":"approve"}' $B/v1/approvals/$AID/decision
curl -s -X POST -H "X-User-Id: bob" -H "Content-Type: application/json" -d '{"decision":"approve","reason":"金额在额度内"}' $B/v1/approvals/$AID/decision
```

5. 模拟 Agent 做完并交付,然后验收:

```bash
curl -s -X POST -H "Content-Type: application/json" -d '{"step":"claim_done"}' $B/v1/dev/tasks/$TID/simulate
curl -s -X POST -H "Content-Type: application/json" -d '{"step":"deliver"}' $B/v1/dev/tasks/$TID/simulate
curl -s -X POST -H "X-User-Id: bob" -H "Content-Type: application/json" -d '{"decision":"accept"}' $B/v1/tasks/$TID/settle
```

6. 看时间线和记分卡:

```bash
curl -s -H "X-User-Id: alice" $B/v1/tasks/$TID/events
curl -s -H "X-User-Id: alice" $B/v1/metrics/scorecard
```

同样的流程在 `backend/tests/test_api.py` 里有自动化版本。

## 现在还没有的

前端先不要依赖这些,后面会补:

- 真实登录和角色。现在任何人写什么 `X-User-Id` 就是谁,「批准人」也没有角色校验,只校验不是发起人。
- 加额度(`LIMIT_REACHED` 的出口)、转人工后的接手和交回、重开已完成任务,这几个操作的正式接口。目前只能用 `simulate`。
- 任务类型配置的增删改。现在只读。
- 等待项到期的自动处理。现在只有在有人去操作一条过期的批准时才会处理。
- 事件流的身份校验和断线续传。
