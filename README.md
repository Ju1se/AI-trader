# AI-trader

价值投资研究和模拟盘执行的 Agent 系统。后端只用 Rust,只连模拟券商,不做真实交易。

- 设计来源:7 张导图,在 [`docs/导图/`](docs/导图/);参考资料在 [`docs/资料/`](docs/资料/)。
- 软件工程图:[`docs/diagrams.md`](docs/diagrams.md),11 张图,编号沿用导图。
- Rust 后端的写法、改进清单和进度:[`docs/rust-architecture.md`](docs/rust-architecture.md)。
- 前端联调:[`docs/api.md`](docs/api.md)。数据库:[`docs/database.md`](docs/database.md)。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/trade-core` | 类型、ID、任务状态机(图 4a)、风险分级、合约、错误类型。无 IO |
| `crates/trade-quant` | 因子打分和否决、时点回测。无 IO |
| `crates/trade-research` | 三个专家和投资委员会。无 IO |
| `crates/trade-gate` | 闸门:纯函数 `decide`(图 3a)和执行顺序 `run`。只定义接口,无 IO |
| `crates/trade-store` | 唯一访问数据库的地方。SQLite 和 PostgreSQL 共用一套 SQL |
| `crates/trade-adapters` | 外部系统:时点行情(读 `fixtures/`)、模拟券商、OpenAI 兼容的模型接口 |
| `crates/trade-server` | 用例(图 0)、模型驱动的执行循环、两个受控工具、`/v1` 接口和事件流、命令行 |
| `config/strategy.toml` | 策略参数:因子权重和阈值、委员会、仓位上限、回测。改了不用重新编译 |
| `config/task_types/` | 任务类型配置:工具的风险分级、上限、放权级别、等待期限 |
| `fixtures/market.json` | 演示和测试用的时点数据 |
| `prompts/agent.md` | 给模型的说明:可用的工具和规则。改了不用重新编译 |
| `migrations/` | 数据库结构 |
| `notebooks/colab.ipynb` | 在 Google Colab 上用本地模型跑全程的笔记本 |

## 常用命令

```bash
# 全部测试:单元、属性测试、和 Python 版的逐项比对、端到端、HTTP 接口
cargo test --workspace

# 演示:回测和各期打分,然后一个任务从受理走到结案,经过两次人工批准
cargo run -p trade-server --bin trade -- demo

# 单独跑回测、单日研究、打分速度
cargo run -p trade-server --bin trade -- backtest
cargo run -p trade-server --bin trade -- research --as-of 2026-02-10
cargo run --release -p trade-server --bin trade -- bench

# 启动接口,带联调接口,数据存在 trade.db
cargo run -p trade-server --bin trade -- serve --dev --db trade.db
```

接口默认在 `http://127.0.0.1:8000`,改用 `--addr`。不给 `--db` 时用内存数据库,进程退出数据就没了。

## 接入模型

模型只负责提议:每一轮提议一个工具调用,写操作都先过闸门,需要人批准的照样要人批准,结果和拒绝原因作为数据告诉模型。任何 OpenAI 兼容的接口都能用,例如本机的 Ollama:

```bash
ollama pull qwen2.5:7b-instruct
cargo run -p trade-server --bin trade -- agent --model-url http://127.0.0.1:11434/v1 --model qwen2.5:7b-instruct --approve-as approver_bob
```

`agent` 用模型跑一个任务并打印对话记录;`--approve-as` 让这个人自动批准,不给就停在第一次批准上。`serve` 加上 `--model-url` 和 `--model` 之后,`POST /v1/tasks/{id}/advance` 由模型驱动。也可以用环境变量 `AITRADER_MODEL_URL`、`AITRADER_MODEL`、`AITRADER_MODEL_API_KEY`。不配置模型时,按固定规则推进。

在 Google Colab 上用 GPU 和本地模型跑全程:打开 [`notebooks/colab.ipynb`](notebooks/colab.ipynb)。

## 用 PostgreSQL

默认用 SQLite。要在 PostgreSQL 上跑,先起本机数据库:

```bash
docker compose up -d db
```

然后把连接串交给服务,或者让存储层的测试在 PostgreSQL 上再跑一遍(每个测试建一个新库):

```bash
cargo run -p trade-server --bin trade -- serve --db postgres://aitrader:aitrader@localhost:5432/aitrader
TRADE_TEST_PG_URL=postgres://aitrader:aitrader@localhost:5432/postgres cargo test -p trade-store
```

## 安全约定

- 模型和专家只提议,所有写操作过闸门。没写进任务类型配置的工具直接拒绝,R4 永远拒绝,R3 必须由发起人和配置管理员以外的人批准确切内容,批准只能用一次、会过期。
- 请求发出后出错,任务冻结为 `OUTCOME_UNKNOWN`,先向券商对账再决定;同一个幂等键不会下第二次单。
- 券商凭证只在服务里,不交给专家或模型。
