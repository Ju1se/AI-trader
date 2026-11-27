# AI-trader 后端

Agent 工作流程的控制面:任务状态机、闸门、批准、动作记录、审计和成本台账。设计依据见 [`docs/diagrams.md`](../docs/diagrams.md)。

这是骨架。接口、数据库和状态机可以运行和联调;Agent 本身还没有接入,由 `/v1/dev` 下的联调接口代替它推进任务。

## 快速开始

需要 Python 3.12 或 3.13。

```bash
cd backend
python3.12 -m venv .venv
.venv/bin/pip install -e ".[dev]"
cp .env.example .env
.venv/bin/uvicorn app.main:app --reload --port 8000
```

启动时会自动建表(Alembic 迁移)并载入演示数据。然后打开:

- 交互式接口文档:<http://localhost:8000/docs>
- 健康检查:<http://localhost:8000/health>

8000 端口被占用时换一个 `--port`。

## 测试

```bash
.venv/bin/python -m pytest -q
```

测试用内存里的 SQLite,不需要任何外部服务。

## 目录

| 路径 | 内容 |
|---|---|
| `app/domain.py` | 13 个任务状态、允许的迁移、风险分级。纯逻辑 |
| `app/gate.py` | 工具调用前的闸门,检查 0 到 5 |
| `app/models.py` | 13 张表 |
| `app/services.py` | 用例:建任务、状态迁移、批准、回答、验收、取消、记分卡 |
| `app/api.py` | HTTP 接口 |
| `app/schemas.py` | 请求和响应的结构,决定 OpenAPI 文档 |
| `app/sim.py` | 联调用:模拟 Agent 的每一步,载入演示数据 |
| `app/adapters.py` | 对外边界的接口定义,以及一个模拟券商 |
| `app/events.py` | 事件流(SSE)用的进程内发布订阅 |
| `migrations/` | Alembic 迁移脚本 |
| `tests/` | 测试 |

## 配置

环境变量都带 `AITRADER_` 前缀,也可以写在 `backend/.env` 里。

| 变量 | 默认值 | 作用 |
|---|---|---|
| `AITRADER_DATABASE_URL` | `sqlite:///./dev.db` | 数据库连接串 |
| `AITRADER_AUTO_MIGRATE` | `true` | 启动时执行迁移。迁移作为部署步骤单独执行时关掉 |
| `AITRADER_SEED_ON_START` | `false` | 启动时清空并载入演示数据。`.env.example` 里打开了它 |
| `AITRADER_ENABLE_DEV_ENDPOINTS` | `true` | 是否挂载 `/v1/dev` 联调接口。上线必须关掉 |
| `AITRADER_CORS_ORIGINS` | `["http://localhost:3000"]` | 允许跨域的前端地址,JSON 数组 |
| `AITRADER_APPROVAL_TTL_HOURS` | `24` | 批准请求多久过期 |
| `AITRADER_INPUT_TTL_HOURS` | `48` | 澄清问题多久过期 |

## 数据库

默认是本地 SQLite 文件,零配置。切到 PostgreSQL 只改连接串:

```bash
docker compose up -d db
.venv/bin/pip install -e ".[postgres]"
```

然后把 `.env` 里的 `AITRADER_DATABASE_URL` 换成 `postgresql+psycopg://aitrader:aitrader@localhost:5432/aitrader`。

改了模型之后生成并执行迁移:

```bash
.venv/bin/alembic revision --autogenerate -m "说明这次改了什么"
.venv/bin/alembic upgrade head
```

表设计、可移植规则和上云步骤见 [`docs/database.md`](../docs/database.md)。

## 给前端

- 接口用法和联调脚本:[`docs/api.md`](../docs/api.md)
- OpenAPI 文件:[`docs/openapi.json`](../docs/openapi.json)。改了接口后重新导出:

```bash
.venv/bin/python -c "import json; from app.main import app; open('../docs/openapi.json','w',encoding='utf-8').write(json.dumps(app.openapi(), ensure_ascii=False, indent=2)+'\n')"
```

## 现状

已经可用:

- 任务的创建和去重、13 个状态和迁移规则、批准(含不能自批、过期即否决)、回答澄清、验收和退回、取消、记分卡、事件流。
- 闸门的判定逻辑和模拟券商,有测试,但还没有接到真实的工具调用上。

还没有做:

- Agent harness 的接入。现在没有任何代码调用模型。
- 闸门的检查 6 到 8:把批准、幂等键登记、执行处再查串进真实的执行路径。
- 真实登录。`X-User-Id` 只是占位,任何人都能冒充任何用户。
- 等待项到期的定时处理。现在只在有人操作过期的批准时才处理。
- 行情数据和真实券商的适配器。
