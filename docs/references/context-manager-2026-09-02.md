# ContextManager 设计文档

> 历史参考快照，来源：`/Users/jiangsumin/codejk/agents/yourai/docs/context-manager.md`。保留旧讨论原文，不代表当前全部采纳；最新分工与待修正问题见 [收敛设计](../session-storage-design.md)。

> 状态：**v1.0 五问收口**（2026-09-02，D-CM1~16 全部决策完毕）。
> 定位：core 机制层，与 loop 同级的产品地基（用户判断：整个产品最核心的地方）。
> 上游依据：`docs/architecture.md` §4.3（trait）、§5.4（方案 D）、§5.5；
> `docs/default-loop.md` D2/D10/D14/D24；`docs/hooks.md`（add_hook_context）。
> 实证来源：hermes-agent（system_prompt.py / prompt_caching.py / prompt_cache_scope.py /
> hermes_state.py / context_compressor.py）与 genai 0.6.5 源码（usage.rs）。

---

## 0. 五问清单（2026-09-01 划定，2026-09-02 全部收口）

| # | 问题 | 状态 | 决策 |
|---|---|---|---|
| Q1 | context 怎么拼接 | ✅ 收口 | D-CM1/2/3（system 不入历史、append-only、4 断点勘误） |
| Q4 | context 怎么存储和持久化 | ✅ 收口 | D-CM8/9/10/11（三表 schema、盘先行、WAL、usage 三轨） |
| Q2 | context 怎么 resume | ✅ 收口 | D-CM12/13/14/15（open 即恢复、装配层职责、单写者） |
| Q3 | context 怎么 compact | ✅ 收口 | D-CM4/5/6/7（trait 供模、五步机制、存储表达） |
| Q5 | context vs session 概念模型 | ✅ 收口 | D-CM16（概念总图 + 四层职责） |

讨论顺序实录：Q1 → Q4 → Q3（用户提前）→ Q2 → Q5。

---

## 1. Q1：context 怎么拼接（讨论中）

> 用户校准（2026-09-01）：拼接的真问题 = **prefix 一致性 → 缓存命中率**——什么放
> system prompt、什么放最前、怎么 compact 才不炸前缀。

### hermes 实证（agent/system_prompt.py, prompt_caching.py, prompt_cache_boundary.py, prompt_cache_scope.py, prompt_builder.py）

**A. system prompt 三层结构，每 session 只构建一次**（模块不变量：只有 compact 才重建，
保前缀缓存）：

| 层 | 内容 | 变化频率 |
|---|---|---|
| stable | identity、工具使用守则、per-model 指导、环境提示、平台提示 | 跨 session 静态 |
| context | 调用方 system_message + context files（AGENTS.md 等）+ workspace 快照 | 每 session 一次 |
| volatile | skills index、memory 快照、时间戳/session/model 行 | 每 turn 可变 |

**B. 请求级 cache 布局（适配层，4 断点）**：system 静态前缀（早 marker）+ system 尾
（晚 marker）+ 最后 2 条非 system 消息（**事务端点** = assistant(tool_calls) 后连续 tool
结果的最后一条，或普通 turn 末条）；tools 布局变体用"最后一条 tool 定义"替换 system 尾
marker。无静态前缀时降级 1+3。幂等 = 先 strip 再 apply；TTL 按 provider clamp
（Qwen/阿里系仅 5m）。

**C. builder 声明式稳定前缀**（#81867）：webhook/cron 消息 = 大静态 scaffold + 小易变尾
（ticket ID/时间戳）。构造时注册边界，cache planner 按注册切 [stable|volatile]，
**不靠运行时 delimiter 猜**；存储仍是原始单串，切分只发生在请求时。

**D. cache key = 压缩 lineage 根**（#79017）：压缩轮转出新物理 session id 后，cache key
仍映射回 lineage 根——轮转不换缓存桶；fork/subagent 子 session 保持隔离 scope。

**E. steer 通道**：mid-turn steer 追加到**最新 tool result 尾部**（OpenAI 系 role 交替下
唯一安全槽），自描述 marker + system prompt 静态信任规则（防注入仿冒）+ 新鲜度规则
（marker 后有 assistant 消息即历史，不算新投递）。

**F. resume 时 static 层重建**（`reconstruct_static_prefix`）：重建 stable 层后必须过
`startswith` 门——不是存储 prompt 的字面前缀（stable 输入变了）就放弃 split、
退回 legacy 布局，**绝不重写历史字节**。

### 设计结论（提案）

1. **system prompt 不入历史**：cm 历史只存 user/assistant/tool_result；system 每次
   `build_request_with(tools, system)` 注入请求头部。写入 cm 契约：**同一 session 内
   system 字节不变，除非 compact**（hermes 不变量的 ours 版）。
2. **历史 append-only**：steer（D7，真实 user 消息）、hook context、工具结果全部尾部追加。
   我们无 role 交替约束 → 不需要 hermes 的 tool-result 尾部通道（D19 已论证），marker
   信任机制也不需要（真实 user role 天然有来源权威）。
3. **build_request 纯函数化**：同（历史, tools, system, options）→ 同字节请求。cm 不在
   build 时做任何"顺手改写"。
4. **适配层布局修正（D19 勘误）**：三断点 → **4 断点**（system 静态前缀 + system 尾 +
   最后 2 事务端点；tools 布局变体），补"事务端点选择器"和"TTL clamp"两个细节。
5. **分层（stable/context/volatile）v1 不做**：config.system_prompt 单层注入；
   分层是组合期字符串拼接的事，v1.1 随 skills/memory 注入（OQ-2）再引入。
6. **cache key lineage**：D19 的 `prompt_cache_key` 目前派生自 session_id——若将来
   compact 轮转 session id 会踩 hermes #79017 同款坑。记入 Q3/Q5：**compact 必须保持
   cache key 稳定**（我们的方案 D 里 compact 不换 session_id，天然规避；文档明示）。

### 决策记录

- **D-CM1（已决策 2026-09-01）**：system prompt **不入历史**。历史 = append-only 的
  纯对话流水（user/assistant/tool_result）；system 是装配期常量，**session 首次 build
  时构建并单独缓存字节**，后续 build 直接复用，绝不重算。
  cm 契约：同一 session 内相同入参的 `build_request_with` 产出逐字节相同的 system
  部分；唯一合法的字节变化点 = compact。禁止实现把时间戳、动态读文件、不稳定排序
  等易变内容拼进 system。
  理由：模型 API 无状态，历史数组即模型记忆本体；system 存进数组头部会让
  换 system = 改历史（破坏 append-only）+ 前缀缓存全灭（system 位于请求第 0 位）+
  compact 需特判。
- **D-CM2（已决策 2026-09-01）**：历史 append-only + build 纯函数化。历史数组只追加，
  任何组件不得改写已入库内容；`build_request_with` 同入参产出逐字节相同的请求，
  不做任何附带改写。唯一合法重写点 = compact。
  理由：历史数组即模型记忆本体（API 无状态），改写 = 篡改记忆；纯函数 build 是
  prefix 缓存命中的前提。
- **D-CM3（已决策 2026-09-01）**：D19 勘误——适配层缓存布局由"三断点"修正为
  **4 断点**：system 静态前缀（早 marker）+ system 尾（晚 marker）+ 最后 2 条事务端点；
  tools 布局变体用"最后一条 tool 定义"替换 system 尾 marker。补事务端点选择器
  （完整工具事务末条 tool result 才是合法端点）与 TTL clamp（Qwen/阿里系仅 5m）。
  依据：hermes prompt_caching.py 实证。

---

## 2. Q4：context 怎么存储和持久化（讨论中）

### hermes 实证（hermes_state.py：SessionDB）

**两表模型**：`sessions`（元数据：id/source/started_at/message_count/tool_call_count/
cwd/model/billing_route/runtime_lock/JSON config）+ `messages`（一表多列的行存）。

**messages 表列清单**（从 INSERT 语句读出，按职能分组）：

| 职能 | 列 |
|---|---|
| 身份/配对 | session_id, role, tool_call_id, tool_calls(JSON), tool_name |
| 内容 | content（多模态 JSON 编码）, **api_content sidecar**（实际发出的字节 ≠ 存储内容时存，缓存回放保真） |
| 记账 | token_count（每行）, finish_reason |
| reasoning sidecar | reasoning, reasoning_content, reasoning_details, codex_*_items |
| **生命周期** | **active**（1=在模型上下文，0=归档）, **compacted**（被压缩归档标记） |
| 平台/展示 | platform_message_id, observed, display_kind, display_metadata（永不进模型上下文） |

**compact 写路径 = 非破坏性 in-place 归档**（`compact_in_place`）：单个事务内
旧消息 `active=0`（留在盘上、仍可全文搜索、可恢复）+ 插入压缩后新 active 行。
**session id 终身不变**（#38763）——cache key 天然稳定，不存在 lineage 轮转问题。

**并发安全**：WAL（多读一写；不支持的文件系统降级 journal_mode=DELETE）；
压缩用 watermark（压缩开始时活跃消息水位，期间新到消息不总结掉、重排到压缩集后）
+ compression_locks 表（租约 TTL，commit 事务内校验锁仍持有）。

**其他**：每条消息一个 INSERT 事务（有 batch 变体）；FTS5 全文索引（CJK 分词）
支撑 session_search；孤儿 tool_call/result 在压缩后处理中清理。

### 待讨论（提案倾向）

1. **两表模型**（sessions 元数据 + messages 行存）——直接采纳，与架构 §5.4
   "SessionManager 管 meta / cm 管历史"对齐
2. **列宽取舍**：hermes 22 列是平台总线（telegram/yuanbao/codex）长年积累；
   v1 收窄为 role/content_json/tool_call_id/token_count/status/created_at，
   genai ChatMessage 整体 JSON 序列化进 content_json 兜未来字段
3. **compact 非破坏归档**（status 枚举）——采纳，同时解决
   "被压缩的存不存"（存，可搜索）与 cache key 稳定（session id 不换）
4. **写入策略**：WAL + 每条 add_message 一个事务，synchronous=NORMAL；批写优化留后
5. **孤儿修复持久化**：D14 合成的 "No result provided" **要落库**（修复后的数组
   才是真数组，否则每次 resume 重修一遍）
6. **usage 记账**：usage_events 流水表（一次 API 调用一行，热列 5 维度 + usage_json
   无损兜底）+ sessions 聚合列；messages.token_count 是估算（compact 预算用），
   usage_events 是权威实值（计费用），不混用

### Schema 草案

```sql
CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,      -- SessionId
  title         TEXT,
  model         TEXT,                  -- 最后使用的模型
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  message_count INTEGER NOT NULL DEFAULT 0,  -- 活跃消息数（compact 后重算）
  -- usage 聚合列（列表页便宜展示；流水表是真相源）
  total_input_tokens          INTEGER NOT NULL DEFAULT 0,
  total_output_tokens         INTEGER NOT NULL DEFAULT 0,
  total_cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  total_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  total_reasoning_tokens      INTEGER NOT NULL DEFAULT 0,
  config_json   TEXT                   -- 其余元数据 JSON 兜底
);

CREATE TABLE messages (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,  -- 行身份，终身不变
  session_id   TEXT NOT NULL,
  kind         TEXT NOT NULL DEFAULT 'message'
               CHECK (kind IN ('message','summary')),  -- summary = compact 摘要行
  role         TEXT NOT NULL,          -- 'user' | 'assistant' | 'tool'
  content_json TEXT NOT NULL,          -- genai ChatMessage 整体 JSON（schema 零迁移）
  tool_call_id TEXT,                   -- tool_result 行回指配对的 call_id
  token_count  INTEGER,                -- 该行 token 估算（compact 尾保护预算用）
  status       TEXT NOT NULL DEFAULT 'active'
               CHECK (status IN ('active','compacted','archived')),
  created_at   INTEGER NOT NULL
);
CREATE INDEX idx_messages_session ON messages(session_id, id);

-- 活跃视图读规则（resume / build_request 共用，D-CM7）：
--   SELECT * FROM messages
--    WHERE session_id=? AND status='active'
--    ORDER BY (kind='summary') DESC, id;
--   不变式：任一时刻至多一个 active summary 行（compact 时旧 summary 同事务归档）；
--   summary 排第 0 位，message 行按 id 序 = 原数组序（尾行无需 re-sequence）。

-- 用量流水：一次 API 调用（StreamEnd captured_usage）一行
-- 字段与 genai 0.6.5 Usage 对齐（usage.rs 实证）：
--   prompt_tokens / cached_tokens / cache_creation_tokens(含 5m/1h TTL 细分)
--   completion_tokens / reasoning_tokens（completion 的子集）
CREATE TABLE usage_events (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id            TEXT NOT NULL,
  input_tokens          INTEGER,       -- usage.prompt_tokens
  output_tokens         INTEGER,       -- usage.completion_tokens
  cache_read_tokens     INTEGER,       -- details.cached_tokens
  cache_creation_tokens INTEGER,       -- details.cache_creation_tokens
  reasoning_tokens      INTEGER,       -- completion details.reasoning_tokens
  usage_json            TEXT NOT NULL, -- genai Usage 整体序列化（无损，含冷门字段）
  created_at            INTEGER NOT NULL
);
CREATE INDEX idx_usage_session ON usage_events(session_id, id);

-- 待决审批（loop 运行态的持久化投影；default-loop.md D27"状态与运输分离"）
-- 发 Ask 同步写、决策落定与 tool result 入库同事务删；resume/前端重连按
-- payload_json 原样重放（Ask 可重放协议要求，见 default-loop.md §5）
CREATE TABLE pending_approvals (
  call_id      TEXT PRIMARY KEY,   -- = Ask id（tool_approval = call_id）
  session_id   TEXT NOT NULL,
  kind         TEXT NOT NULL,      -- 'tool_approval' | 'plan_approval'
  payload_json TEXT NOT NULL,      -- 完整 Ask 载荷（重放原样用）
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER             -- approval_timeout 换算的绝对时刻；NULL = 无限等
);
```

写路径不变量：**盘先行**——`add_message`/`record_usage` 在 COMMIT 成功后才更新
内存态；失败则内存与盘一致地"什么都没发生"（盘 ⊇ 内存）。任何时刻 resume 读盘
即合法数组（WAL 未 commit 事务整条不存在）。

已知约束：`record_usage(&self, usage)` trait 无 model 参数 → 流水表 v1 无 model 列；
按模型归集成本需扩 trait（v1.1）。

### 决策记录

- **D-CM8（已决策 2026-09-02）**：三表 schema 定稿（sessions / messages / usage_events，
  DDL 见上）；content_json 整存不拆列（访问模式只有按序恢复与整行追加，无列查询
  需求；genai 加字段零迁移）；status 单枚举（active/compacted/archived，不用 hermes
  双标志）；kind 列（'message'|'summary'，见 D-CM7）。
- **D-CM9（已决策 2026-09-02）**：**盘先行写序**——add_message / record_usage 在
  COMMIT 成功后才更新内存态；失败则内存与盘一致地"什么都没发生"。
  不变量：**盘 ⊇ 内存**；崩溃最多丢"正在写的那一条"（WAL 未 commit 事务整条
  不存在），resume 读盘即合法配对完整的数组。D3"落库失败熔断"的物理支撑。
- **D-CM10（已决策 2026-09-02）**：`journal_mode=WAL` + `synchronous=NORMAL`。
  进程崩溃不丢已提交事务；OS 级崩溃理论可丢最后几条——对对话记忆是正确取舍
  （FULL 每写一次 fsync，写入延迟×10，不值）。单 db 文件装全部 session。
- **D-CM11（已决策 2026-09-02）**：usage 三轨——usage_events 流水表（真相源，
  5 热列 + usage_json 无损兜底）+ sessions 聚合列（列表页展示）+
  messages.token_count 行级估算（compact 尾预算）。三者各司其职不混用；
  estimate vs authoritative 界限明示。record_usage 无 model 参数 → 流水表 v1 无
  model 列（trait 扩展 v1.1）。

---

## 3. Q2：context 怎么 resume（已收口，见决策记录 D-CM12~15）

### 已定

- 恢复 session = `SqliteContextManager::open(db, session_id)` 重建，构造时 load 活跃
  视图（D-CM7 读规则），没有特殊恢复路径（架构 §5.4）。
- SessionManager 只管元数据（title/时间戳/model）。

### 四个配套问题（提案）

**① 半截 turn 谁修（崩溃窗口的孤儿）**：崩溃可能发生在"assistant 已落库（带
tool_calls）、工具结果未落库"的窗口，盘上尾部即不配对。**归 loop 首步 D14 修复**，
cm.open() 纯 load 不修复不校验。理由：修复是"协议合规"问题不是"记忆容器"职责；
D14 机制已存在（每步请求前扫描），resume 后第一步自然触发；合成结果经普通
add_message 落库（Q4 已决策持久化）→ 闭环。cm 若在 open 时修，混淆读写职责
且同一逻辑要写两处。

**② resume 后工具面重建**：工具定义不在历史（Q1 决策）——**这是装配层职责，
不是 cm 的**。架构 §6.7：Agent = 配置（system prompt + 工具集 + 模型 + loop）。
resume = 调用方用（可能相同的）配置重新构造 Agent + open(session_id)。
风险明示：调用方 resume 时装配不同工具集 → 历史里 tool_calls 引用已不存在的
工具 → 模型可见不一致。cm 无法防；v1 职责约定由调用方保证兼容，
tools fingerprint（存 sessions 表供比对）v1.1 再议。

**③ SessionStart{source:"resume"} hook 时序**：归 **Agent 装配层**（构造后、首个
turn 前），不在 cm.open() 内。理由：cm 是最底层记忆容器，不依赖 hook 系统；
default-loop 的 hook 接点清单里本就没有 SessionStart（它管 turn 内接点，
会话生命周期归 Agent 层）。

**④ 并发打开同一 session_id**：v1 = **单写者约定**（文档明示：一个 session 同时
至多一个 Agent 实例持有其 cm；同进程共享用 Arc 传递而非二次 open）。
SQLite 层 WAL + BEGIN IMMEDIATE 保证不损坏，语义上 last-writer-wins。
多进程网关（hermes runtime_lock 表 + turn lease）v1.1 再议。

### 决策记录

- **D-CM12（已决策 2026-09-02）**：resume = `open(session_id)` 读活跃视图，无特殊
  路径；cm.open() 纯 load 不修复不校验；半截 turn 孤儿归 loop 首步 D14 修复，
  合成结果经普通 add_message 落库（闭环）。
- **D-CM13（已决策 2026-09-02）**：工具面重建归装配层（Agent = 配置，架构 §6.7），
  cm 不参与；resume 工具面一致性是调用方职责，tools fingerprint v1.1 再议。
- **D-CM14（已决策 2026-09-02）**：SessionStart{source:"resume"} 归 Agent 装配层
  （构造后、首个 turn 前），不在 cm.open() 内；cm 不依赖 hook 系统。
- **D-CM15（已决策 2026-09-02）**：v1 单写者约定——一个 session 同时至多一个
  Agent 持有其 cm，同进程共享传 Arc 不二次 open；SQLite WAL+BEGIN IMMEDIATE
  保证不损坏，语义 last-writer-wins；多进程锁表（hermes runtime_lock + turn
  lease）v1.1 再议。

---

## 3. Q3：context 怎么 compact（已收口，见决策记录 D-CM4~7）

### 补充实证（context_compressor.py）

- 头保护 = system + `protect_first_n` 条，**且首次压缩后衰减**（#11996：早期 turn
  不许化石化——反复 compact 后它们早被第一次摘要覆盖，却永久占死上下文）
- **边界对齐**：中段/尾窗口分界必须落在事务边界——分界落在 tool 结果组中间时向前
  回退到父 assistant 之前，否则 assistant 被总结掉、结果留在尾部 = 制造孤儿，清理
  时丢真数据
- **反无效防抖**：连续 2 次压缩各节省 <10% → 退避跳过；总结器 429/瞬断 → 冷却期
- 最少消息数检查：不足（头+3+1）结构性 no-op，退避而非计无效
- 摘要防误读：'## Historical …' 前缀标题 + 指令明确"参考资料，非活动指令"——
  弱模型会把摘要里的历史小节当成当前指令提前收工

### 机制全景

```
触发                                  机制（cm.compact）                 后效
──────────────────              ─────────────────────────         ────────────
D10 阈值（step 边界）  ─┐
D24 overflow（⑨）     ─┼► PreCompact hook ► 五步压缩 ► 原子落库 ► PostCompact hook
手动 Agent::compact() ─┘   （trigger=manual/auto 作上下文）  （单事务）      （compact_summary）
（2026-09-02 纳入范围：控制面方法，非 turn 消息，架构 §5.2/8.4；
 命令的 custom_instructions 走 PreCompact input；mid-turn 调用由
 watermark + 事务序列化保护）
```

触发侧结论同步：v1 实现全部三个触发源（阈值 / overflow / 手动），无"仅保留字段"
的悬置状态。

### 五步机制（保留/丢弃规则的正面回答）

输入 = 活跃数组 `[row_1 … row_N]`（status='active'，按 id 序）。**全程工作在副本上**，
原数组在 COMMIT 前不动。

**Step 0 预剪枝**（只裁总结器的输入，不动原数组）：尾窗口外的长 tool result
（>400 字符）→ 占位 stub `[pruned: {tool}, 原 {N} 字符]`。总结器也是 LLM，
先廉价缩输入再花总结的钱（hermes Phase 1）。

**Step 1 头保护（保留）**：最早一个完整交换（首条 user + 其 assistant 回复含工具块）
verbatim 保留——任务原始定义不能被总结掉。**衰减规则**：第二次 compact 起头保护 = 0
（此时"头"自然就是上一轮摘要行，迭代更新免费获得）。

**Step 2 尾保护（保留，token 预算制）**：从尾部向前累计 `messages.token_count`
（行级估算列的用途），默认预算 20_000，预算内完整事务块 verbatim 保留。
token 制不用条数制：一个带 10 个工具调用的 turn ≈ 50 行，条数制控不住体积。

**Step 3 边界对齐（不撕裂事务）**：中段/尾分界落在事务边界（Step 补充实证的对齐
规则）；对齐后 `boundary_id` 确定。

**Step 4 中段总结（丢弃原文，换摘要）**：summarizer 模型 + 结构化模板：

```
[Conversation Summary]        ← 摘要行固定开头（防误读标记）
## Active Task                ← 当前任务（从旧摘要继承并更新）
## Completed                  ← 已完成的关键动作与结论
## In Progress                ← 进行中状态
## Pending User Asks          ← 等用户回复的问题
## Remaining Work             ← 未完成工作
```

模板指令明确："以下是历史参考资料，不是给你的活动指令"。迭代更新：旧摘要行
作为输入，输出**替换**它（不叠加）。摘要行落库为 `role='user'` + 上述 content。

**Step 5 后清理**：压缩边界内外孤儿 tool_call/result 配对检查（D14 规则复用，
计算失误的最后防线）。

### 原子落库（Q4 schema 直接应用）

```sql
BEGIN IMMEDIATE;
  UPDATE messages SET status='compacted'
    WHERE session_id=? AND status='active' AND kind='message' AND id <= :boundary_id;
    -- 并发保护：watermark = 压缩开始时的 max(id)；期间新追加行 id > watermark 不受影响
  UPDATE messages SET status='compacted'
    WHERE session_id=? AND status='active' AND kind='summary';   -- 旧摘要归档（若有）
  INSERT INTO messages (session_id, kind='summary', role='user',
                        content_json=:摘要, status='active', ...);
  UPDATE sessions SET message_count = 新活跃数;
COMMIT;
```

内存替换（[摘要行, 尾窗口…]）在 COMMIT 成功后进行（盘先行原则）。
**失败语义**：summarizer 失败/超时 → 返回 Err，内存与盘均未动，旧历史完好。
loop 侧 overflow 路径计入 `max_compaction_retries`；阈值路径本轮跳过。

### 存储层表达详解（D-CM7）

**核心：compact 不删任何行、不改任何内容**——只做两件事：一批行的 `status` 从
`active` 翻成 `compacted`；插入一行摘要。

Compact 前的表（id 序 = 数组序）：

```
id    kind      status   role       content_json
100   message   active   user       帮我写爬虫……           ← 头（保护）
101   message   active   assistant  (tool_calls…)           ← 头（保护）
102   message   active   tool       结果……
  …     …        …        …        （中段，要被总结）
188   message   active   user       中间某次对话
─── ← boundary_id = 188（对齐后的分界）───
189   message   active   user       继续优化性能            ← 尾窗口（保护）
190   message   active   assistant  好的，我改了……          ← 尾窗口（保护）
```

Compact 后的表：

```
id    kind      status      role       content_json
100   message   compacted   user       帮我写爬虫……        ← 还在！全文在盘上
101   message   compacted   assistant  (tool_calls…)       ← 还在
  …     …       compacted    …         …                   ← 全部还在
189   message   active      user       继续优化性能        ← 没动
190   message   active      assistant  好的，我改了……      ← 没动
195   summary   active      user       [Conversation Summary] …   ← 新插入
```

活跃视图（build/resume 共用读规则）：

```sql
SELECT * FROM messages
 WHERE session_id=? AND status='active'
 ORDER BY (kind='summary') DESC, id;
-- = [195 summary] ++ [189, 190] = [摘要, 尾窗口原文] —— 正确数组序
```

**"不丢消息"三层保障**：

1. **行级**：全系统无一条 DELETE，content_json 永不改写；唯一 UPDATE 是 status
   翻转。被"压缩掉"的消息只是退出模型视图，字节一个没少。
2. **视图级**：归档行随时可查可恢复（`WHERE status='compacted'`）；将来的
   "撤销 compact"（status 翻回）存储层天然支持；全文搜索覆盖归档内容。
3. **事务级**：五步压缩全程在内存副本上算，最后一把单事务换视图；任何失败/崩溃
   → 回滚 → 压缩前状态原封不动。

**为什么 summary-first 读规则，而不是 hermes 式尾行 re-sequence**：摘要行 id（195）
比尾窗口 id（189/190）大，天真按 id 排会把摘要排到末尾。hermes 用整行克隆 + 新 id
重排（re-sequence）对抗多进程并发写者；我们 v1 单写者不需要——读规则一句
`(kind='summary') DESC` 让摘要恒排第 0 位，**全表保持纯 append-only**（连顺序列都
不改），行身份终身稳定。代价仅是读规则稍复杂。

### trait 间隙裁决（已决策：方案 b，D-CM4）

```rust
// 原：fn compact(&self) -> BoxFuture<Result<()>>;   ← 总结需要模型，签名里没有
// 改（架构 §4.3 勘误）：
fn compact(&self, summarizer: &dyn ModelProvider) -> BoxFuture<Result<()>>;
```

- loop 调用处供模：`config.compact_model: Option<Arc<dyn ModelProvider>>`，
  缺省 = snap.model（主模型）。hermes 用独立廉价辅助模型；策略口留下，v1 默认主模型
- 否 (a) cm 构造时持模型：InMemory cm（subagent 用）不该强制要模型；且"这次压缩用
  哪个模型"是调用期策略不是构造期身份
- 否 (c) 上移到 loop：压缩算法是 cm 内部知识（行布局/status 语义），暴露原语让
  loop 重写 = 每个 loop 重实现一遍

### 防抖（阈值路径三件套）

1. 同一 turn 内 compact 至多一次（latch）
2. 滞回：compact 后 token_count 仍 ≥ 阈值 → 不立即重触发，等下一 turn 边界
3. 反无效：连续 2 次压缩各节省 <10% → 本 session 跳过自动压缩（手动 /compact 不受限）
   ；summarizer 瞬断并入 D15 模型重试退避，不单设冷却

### cache 代价（明示）

compact 一次 = 前缀缓存全失效一次（请求前缀字节全变，全量重 ingest）。
必要代价；session id 不变 → prompt_cache_key 不变（D-CM3 关联，无 lineage 问题）。

### v1 vs hermes 取舍

**采纳**：头保护+衰减、token 尾预算、边界对齐、结构化模板+防误读、迭代更新、
原子事务+watermark、孤儿清理、反无效。
**简化掉**：lean mode chunk digests、skill marker 追踪、proactive prune（两次压缩
之间的持续剪枝）、压缩锁租约表（v1 单进程单写者，watermark 足够）、失败路由 pin、
可行性跳过。

### 决策点

1. trait 改 `compact(summarizer: &dyn ModelProvider)`（方案 b）+ `config.compact_model`
2. 头保护衰减规则（二次 compact 起 = 0）
3. 尾预算固定 20_000（v1 不配置化）
4. 防抖三件套（latch / 滞回 / 反无效）
5. 摘要模板五节版（v1 定稿）

### 决策记录

- **D-CM4（已决策 2026-09-02）**：trait 勘误——`compact(&self)` 改为
  `compact(&self, summarizer: &dyn ModelProvider)`；loop 侧新增
  `config.compact_model: Option<Arc<dyn ModelProvider>>`（缺省 = snap.model）。
- **D-CM5（已决策 2026-09-02）**：五步机制定稿——预剪枝（stub 只裁总结器输入）、
  头保护（衰减：二次 compact 起归零）、尾保护（token 预算固定 20_000）、边界对齐
  （不撕裂事务）、中段总结（五节模板 + 防误读标记 + 迭代替换）、后清理（孤儿配对）。
- **D-CM6（已决策 2026-09-02）**：落库 = 单事务视图切换（见 §2 Schema 的 kind 列与
  读规则）；防抖三件套（turn 内 latch / 滞回 / 连续 2 次节省 <10% 关自动压缩）；
  失败 = 全回滚，内存盘皆未动。
- **D-CM7（已决策 2026-09-02）**：**compact 的存储表达 = status 翻转 + 摘要行插入，
  零删除零改写**。messages 表加 `kind` 列（'message' | 'summary'）；活跃视图读规则
  `ORDER BY (kind='summary') DESC, id`——至多一个 active summary 行排最前，其余
  message 行按 id 序即数组序。全表 append-only（唯一 UPDATE 是 status 翻转），
  行身份（id）终身不变，不做 hermes 式尾行 re-sequence（理由：hermes 需要它对抗
  多进程并发写者；我们 v1 单写者 + summary-first 读规则已保证视图有序）。

---

## 5. Q5：context vs session 概念模型（已收口，D-CM16）

### 完整概念图（经 Q1/Q4/Q3/Q2 讨论后定稿）

```
┌─ 装配层（Agent = 配置：system prompt + 工具集 + 模型 + loop）     架构 §6.7
│    · 工具面重建（D-CM13）、SessionStart hook（D-CM14）
│    · 持有 session 的唯一 Agent 实例（D-CM15 单写者）
├─ loop（DefaultLoop 等）
│    · compact 触发（阈值/overflow）+ summarizer 供模（D-CM4）
│    · D14 孤儿修复（resume 后首步自然触发，D-CM12）
│    · turn 内 hook 接点（PreToolUse/PostToolUse/…/Pre/PostCompact）
├─ ContextManager（trait，本文件的主体）＝ 模型记忆本体的管理者
│    · 历史数组：append-only、build 纯函数（D-CM2）
│    · system prompt 装配期常量注入，不入历史（D-CM1）
│    · compact 机制（五步 + 单事务视图切换，D-CM5~7）
│    · record_usage / token_count
│    ├─ InMemoryContextManager：纯内存（subagent 子 agent 用）
│    └─ SqliteContextManager：构造 load 活跃视图 + 每写必存（D-CM8~11）
└─ 存储（SQLite）：sessions + messages + usage_events 三表
     · WAL + NORMAL；盘 ⊇ 内存（D-CM9）；零删除零改写（D-CM7）

SessionId ＝ 横切身份标识（不是对象、不是第二层）：
  cm 绑定 ↔ sessions 行 ↔ messages.session_id ↔ usage_events.session_id
  ↔ prompt_cache_key 派生源 ↔ SessionManager 列表键
```

**SessionManager 与 ContextManager 的分工**：SessionManager 只碰 `sessions` 表的
元数据列（title/model/created_at…），**永远不读写 messages**；ContextManager 只碰
历史与 usage，不碰 title。两者靠 session_id 关联，无对象级耦合。

**用户原始直觉的最终修正**："内存部分是 context，持久化是 session" →
运行态与持久态是**同一个 ContextManager 的两种实现形态**（trait 一致、契约一致）；
session 是横切其上的身份标识 + SessionManager 里的一行元数据。
**Context（容器）≠ ContextManager（对话历史管理）**——架构 §4.2 命名澄清保留。

### 决策记录

- **D-CM16（已决策 2026-09-02）**：概念模型定稿——ContextManager = 记忆本体管理者
  （trait + 两实现）；SessionId = 横切身份；SessionManager = 纯元数据，与 cm
  无对象级耦合；四层职责图（装配 / loop / cm / 存储）如上。
