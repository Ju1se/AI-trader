# Agent 工作流程图集

用图把七张导图描述的流程梳理一遍,作为动手写代码之前的设计依据。图用 Mermaid 写成,GitHub 会直接渲染。

- 流程的文字说明见蓝图页面;本文件只放图和读图要点。
- 阶段名沿用导图里的大写动词。来源写法 `3.2.4` 表示第 3 张图第 2 个分支第 4 条。
- 图里的实线是主路径,虚线是异常或旁路。

| 图 | 类型 | 对应导图 |
|---|---|---|
| 图 0 总体活动图 | 活动图 | 图 7 Workflow(整合前六张) |
| 图 1 职责泳道图 | 泳道图 | 图 1 Control |
| 图 2 定义阶段活动图 | 活动图 | 图 2 Decisions |
| 图 3a 闸门判定活动图 | 活动图 | 图 3 Authorize |
| 图 3b 批准时序图 | 时序图 | 图 3 Authorize |
| 图 4a 任务状态机 | 状态机图 | 图 4 Persist |
| 图 4b 恢复活动图 | 活动图 | 图 4 Persist |
| 图 5 场景时序图 | 时序图 | 图 5 Workflows |
| 图 6 收尾与度量活动图 | 活动图 | 图 6 Economics |
| 图 7 组件图 | 组件图 | 技术栈 |
| 图 8 数据模型 | ER 图 | 图 1 的 1.5.2,图 4 |

## 图 0 总体活动图

对应图 7 的五个分支:PREPARE、ADMIT、FRAME、ACT、CLOSE。设计期只做一次,运行期每个任务走一遍,学习期的结论回头改设计。

```mermaid
flowchart TB
  P["1 PREPARE 设计期,每类任务一次<br/>选最简单且能过评测的形态<br/>定角色、风险分级、上限、完成标准<br/>评测成功案例和对抗案例"]
  START(["触发事件:消息、卡片状态变化、定时、回调"])
  A["2 ADMIT 受理<br/>INTAKE 去重,建任务记录<br/>IDENTIFY 认证,定执行身份<br/>ISOLATE 沙箱,启用上限"]
  F["3 FRAME 定义<br/>RETRIEVE 取上下文<br/>CLARIFY 定任务合约<br/>ROUTE · DECOMPOSE · DELEGATE"]

  subgraph ACT["4 ACT 执行循环,每个动作一轮"]
    X1["模型提议一个动作"] --> X2{"AUTHORIZE<br/>闸门"}
    X2 -->|"允许"| X4["EXECUTE<br/>先登记,再执行"]
    X2 -->|"需批准"| X3["APPROVE<br/>人批准确切内容"]
    X3 -->|"已批准"| X4
    X2 -->|"拒绝,原因回给模型"| X1
    X4 --> X5["VERIFY<br/>单步回读"]
    X5 --> X6{"还要动作?"}
    X6 -->|"要,REPLAN"| X1
  end

  C1{"5 CLOSE<br/>VERIFY 结果达标?"}
  C2["DELIVER 交付和清单"]
  C3["SETTLE 等确认,结案"]
  DONE(["已完成"])
  LEARN["学习期<br/>CALCULATE 算单位成本<br/>ORGANIZE 整理记忆<br/>FALSIFY 复查设计"]
  HUMAN(["ESCALATE 转人工"])

  P -->|"达到门槛,发布任务类型配置"| A
  START --> A
  A --> F
  F --> X1
  X6 -->|"不要,模型声明完成"| C1
  C1 -->|"未达标,返工"| X1
  C1 -->|"达标或部分达标"| C2
  C2 --> C3
  C3 --> DONE
  DONE --> LEARN
  LEARN -.->|"改配置或换形态"| P
  F -.->|"歧义问不清"| HUMAN
  X3 -.->|"否决或过期且绕不开"| HUMAN
  X5 -.->|"结果未知且查不清"| HUMAN
```

## 图 1 职责泳道图:一个动作经过哪几层

对应图 1 的职责分层(1.3)和最小循环(1.4)。模型只提议,Harness 排序和记账,企业后端做决定并执行,人只决定不可逆的事,效果发生在业务系统里。

```mermaid
flowchart TB
  subgraph L1["模型:只提议"]
    M1["提议一次工具调用<br/>结构化参数"]
    M2["把结果当数据读<br/>继续或改计划"]
  end
  subgraph L2["Harness:排序和记账"]
    H1{"结构、限额<br/>硬拒绝"}
    H2["登记意图和幂等键"]
    H3["回读核对<br/>写日志和成本"]
  end
  subgraph L3["企业后端:决定并执行"]
    B1{"权限、业务规则<br/>风险分级"}
    B2["执行处再查一次<br/>持凭证执行"]
  end
  subgraph L4["人:只决定不可逆的事"]
    P1["批准确切内容<br/>绑定哈希,会过期"]
  end
  subgraph L5["业务系统:效果发生的地方"]
    S1["状态改变"]
  end

  M1 --> H1
  H1 -->|"通过"| B1
  H1 -.->|"拒绝"| M2
  B1 -->|"允许"| H2
  B1 -->|"需批准"| P1
  B1 -.->|"拒绝"| M2
  P1 -->|"已批准"| H2
  H2 --> B2
  B2 --> S1
  S1 -->|"独立回读"| H3
  H3 --> M2
  M2 -->|"下一个动作"| M1
```

## 图 2 定义阶段活动图

对应图 2 的 CLARIFY、DECOMPOSE、REPLAN、ROUTE、DELEGATE。新增的部分是向发起人提问,以及不可撤销步骤后置。

```mermaid
flowchart TB
  IN(["受理完成"]) --> R["RETRIEVE<br/>只取有权看的相关记录<br/>外部内容标成数据"]
  R --> C["CLARIFY<br/>模型起草任务合约<br/>目标、范围、完成标准"]
  C --> Q{"有歧义?"}
  Q -->|"没有"| PIN["合约落盘,带版本"]
  Q -->|"有,不影响金额、对象、删除"| ASM["记为假设"]
  ASM --> PIN
  Q -->|"有,影响金额、对象、删除"| ASK["问发起人<br/>状态改为等回答"]
  ASK -->|"答复"| C
  ASK -.->|"到期没答"| ESC(["转人工或取消"])
  PIN --> RT["ROUTE<br/>便宜模型做分类,强模型做推理<br/>精确计算交给代码"]
  RT --> SH{"固定流程<br/>就够用?"}
  SH -->|"够"| GO(["进入执行循环"])
  SH -->|"不够"| D["DECOMPOSE<br/>拆步骤、标依赖<br/>不可撤销的排在校验和批准之后"]
  D --> PAR{"需要并行或<br/>隔离上下文?"}
  PAR -->|"不需要"| GO
  PAR -->|"需要"| DL["DELEGATE<br/>写任务书,worker 只读<br/>结果按引用交回"]
  DL --> GO
  GO -.-> RP{"REPLAN<br/>结果与计划的<br/>假设冲突?"}
  RP -->|"改动在合约和上限内"| D
  RP -->|"超出合约"| C
  RP -.->|"触到上限"| LIM(["停下并保存进度"])
```

## 图 3a 闸门判定活动图

对应图 3 的 IDENTIFY、AUTHORIZE、APPROVE。每次调用按固定顺序过九道检查,结果只有允许、拒绝、交人批准三种。

```mermaid
flowchart TB
  P(["模型提议的一次调用"]) --> G0{"0 工具挂在<br/>配置里?"}
  G0 -->|"否"| DENY
  G0 -->|"是"| G1{"1 上限没用完<br/>且任务未冻结?"}
  G1 -->|"否"| STOP(["停止<br/>触上限或结果未知"])
  G1 -->|"是"| G2{"2 参数结构合法?<br/>身份字段由 Harness 填"}
  G2 -->|"否"| DENY
  G2 -->|"是"| G3{"3 命中硬拒绝?<br/>永不允许或范围外"}
  G3 -->|"是"| DENY
  G3 -->|"否"| G4{"4 权限通过?<br/>角色加属性"}
  G4 -->|"否"| DENY
  G4 -->|"是"| G5{"5 风险分级<br/>和业务规则"}
  G5 -->|"R0 读取,R1 工作区写入"| G7
  G5 -->|"R2 限额内,走后端校验的窄工具"| G7
  G5 -->|"R3 不可逆、超限、批量、新对手"| G6["6 人工批准"]
  G5 -->|"R4,或没有例外通道"| DENY
  G6 -->|"批准且未过期"| G7["7 登记动作记录<br/>和幂等键"]
  G6 -->|"否决或过期"| DENY
  G7 --> G8{"8 执行处再查<br/>权限、批准、哈希一致?"}
  G8 -->|"是"| RUN(["执行"])
  G8 -->|"否"| DENY(["拒绝<br/>原因作为工具结果回给模型"])
  ERR["任何一道检查自己出错"] -.-> DENY
```

## 图 3b 批准时序图

对应图 3 的 APPROVE。批准有两种形式:在流程内批准,或在业务系统里批准。付款类优先用后一种,Agent 只创建待批准的请求。

```mermaid
sequenceDiagram
  autonumber
  participant M as 模型
  participant H as Harness
  participant B as 企业后端
  actor A as 批准人
  participant S as 业务系统

  M->>H: 提议调用,含工具、对象、参数
  H->>B: 过闸门
  B-->>H: 需要人批准
  alt 流程内批准
    B->>B: 建批准记录,存内容哈希、规则、过期时间
    B->>A: 通知,附代码整理的证据
    Note over H: 任务状态为等批准,进程可以退出
    A->>B: 批准,批准人不是发起人
    B->>H: 唤醒并重过闸门
    H->>B: 登记意图和幂等键
    B->>B: 重算哈希,重查权限和规则
    B->>S: 执行
    S-->>B: 回执
  else 业务系统内批准
    H->>B: 登记意图和幂等键
    B->>S: 创建待批准的请求
    S-->>B: 请求编号,状态为待批准
    Note over H: 任务状态为等批准,进程可以退出
    A->>S: 在业务系统里批准并完成强认证
    S->>S: 自己执行
    H->>S: 读取请求状态
    S-->>H: 已批准并执行
    H->>B: 按读到的事实补写批准记录和审计记录
  end
  H->>S: 独立回读结果
  S-->>H: 当前状态
  H->>M: 结果作为数据返回
  opt 否决或过期
    B-->>H: 理由回给模型,能绕开就改计划,否则任务以被否决结束
  end
```

## 图 4a 任务状态机

对应图 4 的 TRACK。导图给了待处理、运行中、等待批准、失败或完成;其余八个状态是补充的。每个等待状态都有负责人、期限和到期动作。

```mermaid
stateDiagram-v2
  state "排队 QUEUED" as QUEUED
  state "运行中 RUNNING" as RUNNING
  state "等回答 AWAITING_INPUT" as AWAITING_INPUT
  state "等批准 AWAITING_APPROVAL" as AWAITING_APPROVAL
  state "结果未知 OUTCOME_UNKNOWN" as OUTCOME_UNKNOWN
  state "触上限 LIMIT_REACHED" as LIMIT_REACHED
  state "验收中 VERIFYING" as VERIFYING
  state "待确认 SETTLING" as SETTLING
  state "已完成 COMPLETED" as COMPLETED
  state "已失败 FAILED" as FAILED
  state "转人工 HANDED_OFF" as HANDED_OFF
  state "被否决 DECLINED" as DECLINED
  state "已取消 CANCELLED" as CANCELLED

  [*] --> QUEUED: 受理,去重键没出现过
  QUEUED --> RUNNING: Harness 认领,身份和上限就绪
  QUEUED --> FAILED: 认证或隔离失败,排队超时

  RUNNING --> AWAITING_INPUT: 向发起人提问
  AWAITING_INPUT --> RUNNING: 有权回答的人答了
  AWAITING_INPUT --> HANDED_OFF: 到期没答

  RUNNING --> AWAITING_APPROVAL: 闸门要求人批准
  AWAITING_APPROVAL --> RUNNING: 批准且哈希一致,或否决后可绕开
  AWAITING_APPROVAL --> DECLINED: 否决或过期,且绕不开

  RUNNING --> OUTCOME_UNKNOWN: 有后果的调用没有明确结果
  OUTCOME_UNKNOWN --> RUNNING: 按键查到确定状态
  OUTCOME_UNKNOWN --> HANDED_OFF: 期限内查不清

  RUNNING --> LIMIT_REACHED: 轮数、花费或时间用完
  LIMIT_REACHED --> RUNNING: 负责人批了新额度
  LIMIT_REACHED --> VERIFYING: 验收并交付已完成的部分

  RUNNING --> VERIFYING: 模型不再提动作
  VERIFYING --> RUNNING: 未达标,返工
  VERIFYING --> SETTLING: 达标或部分达标,已交付
  VERIFYING --> FAILED: 返工次数用完且没有可交付的成果

  SETTLING --> COMPLETED: 验收人接受、业务系统报告完成或窗口期满
  SETTLING --> RUNNING: 被退回
  COMPLETED --> RUNNING: 事后发现不对,重开

  RUNNING --> FAILED: 明确且不可恢复的错误,没有未决动作
  RUNNING --> HANDED_OFF: 命中转人工规则
  RUNNING --> CANCELLED: 发起人或运维取消
  HANDED_OFF --> RUNNING: 人答完问题交回,重过闸门
```

终止状态有五个:已完成、已失败、转人工、被否决、已取消。每个终止出口都要通知发起人并关闭成本台账。为了图面清楚,没有画它们到终点的箭头。

## 图 4b 恢复活动图

对应图 4 的 RECOVER。Harness 每次启动都走这条路径,首次启动只是没有东西要对账的特例。

```mermaid
flowchart TB
  T(["Harness 启动<br/>首次、崩溃后重启、等待结束"]) --> R1["读任务记录、合约、计划、会话日志"]
  R1 --> R2{"有发出去但<br/>没有结果的调用?"}
  R2 -->|"没有"| R6
  R2 -->|"有"| R3["状态改为结果未知<br/>冻结一切新动作"]
  R3 --> R4["到业务系统按幂等键查询"]
  R4 --> R5{"查到什么?"}
  R5 -->|"已生效"| OK["动作记录标为已确认<br/>补写结果"]
  R5 -->|"未生效"| RE["用同一个键重发<br/>原批准仍然有效"]
  R5 -.->|"期限内查不清"| ESC(["转人工<br/>不换新键重发"])
  OK --> R6["重过闸门<br/>等待期间权限和批准可能变了"]
  RE --> R6
  R6 --> R7{"任务记录里<br/>的状态"}
  R7 -->|"运行中"| C1(["继续执行循环"])
  R7 -->|"等批准、等回答"| C2["读批准记录或业务系统里的请求状态<br/>按读到的事实推进"]
  R7 -->|"验收中、待确认"| C3(["继续收尾"])
  C2 --> C1
```

## 图 5 场景时序图:研究加模拟下单

对应图 5 的四类工作。这里画的是本仓库建议的第一条路径:研究一个标的,生成下单请求,人批准,模拟盘执行,回读核对。券商一侧先接模拟盘。

```mermaid
sequenceDiagram
  autonumber
  actor U as 发起人
  participant FE as 前端
  participant BE as 后端控制面
  participant AG as Harness 和模型
  participant MD as 行情和资料源
  actor AP as 批准人
  participant BR as 模拟券商

  U->>FE: 提交研究请求
  FE->>BE: 创建任务
  BE->>BE: INTAKE 去重,IDENTIFY 认证,ISOLATE 设上限
  BE->>AG: 启动会话
  AG->>U: CLARIFY 提问,经前端转达
  U-->>AG: 回答
  BE->>BE: 任务合约落盘
  loop 研究阶段,每个动作一轮
    AG->>BE: 提议读取行情或资料,属于 R0
    BE->>MD: 闸门放行后读取
    MD-->>BE: 数据,带来源和时间
    BE-->>AG: 结果标为数据
  end
  AG->>BE: 提议创建下单请求,含标的、方向、数量、限价
  BE->>BE: 闸门判为 R3,建批准记录
  BE->>AP: 批准收件箱显示确切内容、依据、与持仓的差异
  AP->>BE: 批准
  BE->>BE: 登记意图和幂等键,执行处再查
  BE->>BR: 提交模拟订单,带幂等键
  BR-->>BE: 订单回执
  BE->>BR: 独立回读订单状态
  BR-->>BE: 已成交或已挂单
  BE-->>AG: 核对过的结果
  AG->>BE: 不再提动作,声明完成
  BE->>BE: VERIFY 逐条检查合约,DELIVER 生成报告和清单
  BE->>FE: 报告、引用、订单记录
  U->>FE: 验收,任务结案
```

## 图 6 收尾与度量活动图

对应图 6 的 CALCULATE 和 FALSIFY,以及图 7 的 CLOSE。虚线表示向成本台账写数据的时点,事后重建的数字会漏掉失败的尝试和人工分钟数。

```mermaid
flowchart TB
  V{"VERIFY 结果验收<br/>代码查业务系统<br/>独立评分者做判断"}
  V -->|"逐条达标"| D["DELIVER<br/>完整交付,附清单"]
  V -->|"返工次数用完,有可交付成果"| DP["DELIVER<br/>部分交付,标出缺口"]
  V -.->|"未达标,次数未用完"| BACK(["回执行循环返工"])
  D --> S{"SETTLE<br/>结果被确认?"}
  DP --> S
  S -->|"验收人接受、业务系统报告完成、窗口期满"| DONE(["已完成<br/>记下结果类别"])
  S -.->|"被退回"| BACK
  DONE -.->|"事后发现不对"| REOPEN(["重开<br/>从成功数里扣掉"])

  subgraph LEDGER["成本台账,每个任务一份"]
    L1["模型和工具成本<br/>含 worker、评分者、恢复"]
    L2["人工分钟数<br/>提问、批准、验收、交接"]
    L3["结果类别和重开事件"]
  end
  LOOP(["执行循环每一轮"]) -.-> L1
  APPR(["每次提问和批准"]) -.-> L2
  V -.-> L1
  S -.-> L2
  DONE -.-> L3
  REOPEN -.-> L3

  LEDGER --> CALC["CALCULATE<br/>单个成功任务成本 =<br/>全部尝试的成本 ÷ 已结案且未重开的任务数"]
  CALC --> FAL{"FALSIFY<br/>更简单的形态<br/>够用吗?"}
  FAL -->|"够,且更便宜"| DOWN(["降一档形态"])
  FAL -->|"不够"| KEEP(["保持<br/>按数据升降放权级别"])
  DONE --> ORG["ORGANIZE<br/>从已结案任务提炼记忆<br/>先审后用"]
```

## 图 7 组件图

按选定的技术栈画。闸门、记录和工具执行都在你自己的后端里;Anthropic 托管的只有循环、会话日志和沙箱,隔在适配层后面。

```mermaid
flowchart LR
  subgraph CLIENT["浏览器"]
    UI["Next.js 前端<br/>看板、详情、批准收件箱<br/>记分卡、配置"]
  end
  subgraph SERVER["你的后端,Python"]
    API["FastAPI 接口<br/>任务、批准、交付"]
    ADAPTER["Harness 适配层"]
    GATE["闸门和策略<br/>风险分级、业务规则"]
    TOOLS["自定义工具执行器<br/>行情适配器、券商适配器"]
    WORKER["后台 worker<br/>过期、对账、续做"]
  end
  DB[("PostgreSQL<br/>任务、合约、动作<br/>批准、审计、成本")]
  subgraph HOSTED["Anthropic 托管"]
    CMA["Claude Managed Agents<br/>循环、会话日志、沙箱"]
  end
  MD["行情和资料源"]
  BR["模拟券商"]

  UI <-->|"HTTPS 和 SSE 实时更新"| API
  API <--> DB
  WORKER <--> DB
  GATE <--> DB
  API <--> ADAPTER
  ADAPTER <-->|"会话事件、工具调用和结果"| CMA
  ADAPTER -->|"每次工具调用先过闸门"| GATE
  GATE -->|"放行"| TOOLS
  TOOLS --> MD
  TOOLS --> BR
  WORKER --> ADAPTER
```

## 图 8 数据模型

对应图 1 的「业务状态记在模型之外」(1.5.2)和图 4 的记录。字段只列关键的,落库时再细化。

```mermaid
erDiagram
  TASK_TYPE_PROFILE ||--o{ TASK : "配置"
  TASK ||--|| AUTHORITY : "绑定"
  TASK ||--o{ TASK_CONTRACT : "版本"
  TASK ||--o{ PLAN : "版本"
  TASK ||--o{ SESSION_EVENT : "日志"
  TASK ||--o{ WAIT : "等待"
  TASK ||--o{ ACTION : "动作"
  ACTION ||--o| APPROVAL : "需要时"
  ACTION ||--o{ AUDIT_RECORD : "留痕"
  TASK ||--o{ VERIFICATION : "验收"
  TASK ||--o| DELIVERY : "交付"
  TASK ||--o{ COST_ENTRY : "成本"

  TASK_TYPE_PROFILE {
    string id PK
    int version
    string shape
    json tool_risk_tiers
    json limits
    string autonomy_level
  }
  TASK {
    string id PK
    string dedupe_key UK
    string state
    int state_version
    string requester
    string acting_principal
    string owner
    datetime due_at
  }
  AUTHORITY {
    string task_id FK
    json roles_and_attributes
    json data_scope
    string credential_ref
    datetime expires_at
  }
  TASK_CONTRACT {
    string task_id FK
    int version
    string goal
    json success_criteria
    json assumptions
  }
  PLAN {
    string task_id FK
    int version
    json steps
  }
  SESSION_EVENT {
    string task_id FK
    string event_type
    json payload
    datetime at
  }
  WAIT {
    string task_id FK
    string kind
    string addressee
    datetime expires_at
    string on_expiry
  }
  ACTION {
    string op_id PK
    string task_id FK
    string idempotency_key UK
    string args_hash
    string risk_tier
    string status
    string external_ref
  }
  APPROVAL {
    string id PK
    string op_id FK
    string args_hash
    string approver
    datetime expires_at
    string status
  }
  AUDIT_RECORD {
    string id PK
    string op_id FK
    string requester
    string acting_principal
    string approver
    string decision
    string rule_version
  }
  VERIFICATION {
    string task_id FK
    string criterion
    string verdict
    json evidence
  }
  DELIVERY {
    string task_id FK
    string outcome_class
    json manifest
  }
  COST_ENTRY {
    string task_id FK
    string stage
    string kind
    decimal amount
    int human_minutes
  }
```
