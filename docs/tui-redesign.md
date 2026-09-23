# TUI 界面重设计与 /models 功能（实施文档）

> 历史设计记录。当前规范以 [TUI review 修订设计](./tui-review-design.md) 为准；尤其是信息层级、覆盖层输入、费用展示及缓存策略。
> 面向：维护 TUI、模型切换与会话选择功能的开发者
> 当前代码：`apps/yourai-tui`、`crates/yourai-harness`。实现已包含 `/models`、`/sessions`、裸 `--resume` launcher、工具卡片、动态编辑器、主题与统计覆盖层；符号和测试以源码为准。
> 文中“代码现状勘察”记录的是重构前基线，行号已经漂移，不应视为当前源码说明。

---

## 0. 要做什么（TL;DR）

四件事：

1. **拆掉常驻右侧栏**：遥测搬进 `Ctrl-B` 仪表盘覆盖层（复用现有 `help()` 覆盖层模式），对话区全宽。
2. **一行状态栏承载关键信息**：模型 / context 进度条 / token / todo 计数 / 权限；Todo 列表移到**右侧专栏目板**（见 §4.1 样式）。
3. **新增 `/models`**：打开模型选择器，切换当前会话使用的模型（含 variant），涉及 harness 侧模型句柄可换（见 §5）。
4. **新增 `/sessions` + 启动会话选择器**：会话内查看/搜索/切换会话；`--resume` 不带 ID 时进入以 session title 为主体的可搜索启动页（见 §6）。
5. **工具调用卡片化 + 运行感知 + 视觉升级**：每个工具默认渲染类型化效果预览（edit 显示带行号槽和着底色的 diff，shell 跟随滚动输出），而非一行折叠条目；输入框上方常驻"呼吸灯"运行指示条；输入框动态增高；整体视觉去 ASCII 化（圆角、强调条、spinner）；主题支持跟随系统 + 11 套 IDE 调色板；welcome 界面重做为 neofetch 风双栏（见 §4.3–§4.8）。

设计原则：对话是唯一主角；信息分三层（L0 状态栏常显 → L1 Todo 面板/输入提示 → L2 Ctrl-B 仪表盘按需）；只显示可行动的指标；颜色即语义（绿=成功/正常，黄=告警，红=错误，橙专属于权限/YOLO，灰=次要）。

---

## 1. 代码现状勘察（事实清单）

### render.rs（apps/yourai-tui/src/ui/render.rs，约 1310 行）

- `Renderer::draw()` 布局：`if v.sidebar && area.width >= 110` → 横向 `[Min(50), Length(29)]`，否则全宽。纵向 rows：`[Min(1) transcript, ask_height, 0, input_height, 1 footer]`。
- `sidebar()`（约 624-808 行）：渲染 SESSION OVERVIEW / TASKS（进度条+计数，无列表）/ CONTEXT·ESTIMATED / REQUESTS·THIS RUN / SESSION TOKENS / PERMISSIONS / WORKSPACE / SESSION ID。
- **`draw_todos()`（约 841-872 行）是死代码：定义后从未被调用。** Todo dock 曾存在于输入框上方，现已被移除。
- footer（约 338-395 行）：左侧 activity/status 文本，右侧可点击按钮排 `[F1 /] [^B] [dark]`，配套 `FooterAction` 枚举（24-28 行）与 `footer_hits` 命中区。
- `timeline()`（约 873-968 行）：折叠工具条目已是单行 `▸ ✓ shell · 0s {summary}`；thinking 折叠为 `▸ Thinking · N chars`；展开用 `│ ` 前缀；edit 输出 `+`/`-` 行已绿/红着色；**每个 item 后一律追加空行**。
- transcript 顶部边框标题 `✦ {session title}`，右侧 `N new · Ctrl-End follows`。
- `help()`（约 829-840 行）：居中文本覆盖层，`Clear` + bordered Paragraph，Esc/F1 关闭——**仪表盘与模型选择器复用这个模式**。

### state.rs（src/ui/state.rs）

- `View` 已有全部所需数据：`model_metrics: BudgetSnapshot`、`context_usage: Option<ContextUsage>`、`usage: Usage`（session 累计 in/out/total）、`recorded_responses`、`todos: Vec<Todo>`（`Todo{id, text, completed}`）、`since`、`retry`。
- `View.sidebar: bool`（默认 true）、`View.todos_expanded: bool`（默认 true）、`View.todo_scroll`。
- `activity(v)`（render.rs 515-547）：返回 "Waiting for approval" / "Running shell" / retry 文案等，直接复用。

### ui.rs（src/ui.rs，事件循环）

- 键位：`^B` 切 sidebar、`^T` 切 `todos_expanded`（dock 已不渲染，此键当前无效）、`^Y` 换主题、`^O/^R` 展开块/thinking、`F6` 选块、`F1` help、`Ctrl-End` follow。
- **关键坑：约 545 行 `if view.sidebar && context_refreshed.elapsed() >= 1s` 才刷新 `view.context_usage` 与 `recorded_responses`**。拆侧栏后必须去掉 `view.sidebar &&` 条件（保留 1s 节流）。
- 鼠标：`begin_selection` 使用 transcript 或 sidebar 区域；`todo_hit`/`todo_area` 现有字段供点击/滚动复用。

### 模型配置与流转（/models 相关）

- `config.rs`：`Config.provider: BTreeMap<String, ProviderConfig>`；`ProviderConfig.models: BTreeMap<String, ModelConfig>`；`ModelConfig { id: Option<String>, limit: Limits, options: Map<String,Value>, headers, variants: BTreeMap<String, Map<String,Value>> }`（约 48-86 行）。variant 含 `"disabled": true` 的需过滤。
- `Config.model` 形如 `"provider/model"`；`Config::resolve(&mut self, variant: Option<&str>) -> Result<Arc<GenaiModel>, Error>`（约 157 行起）：校验并构建模型，副作用是写回 `self.context.context_window / input_limit`（约 207-208 行）。
- `main.rs`：`let model = config.resolve(...)` → `Harness::open(hc, model)`；此后 `ui::run(&harness, &config.model, ...)`。**运行时没有换模型的通道。**
- `DefaultLoop`（crates/yourai-harness/src/default_loop/mod.rs:136）：`model: Arc<dyn ModelProvider>`，启动后固定。
- 每个请求的 context 预算来自构建期 `hc.context_policy`（换模型后其窗口/输入限额不同，需要一并更新）。

### 会话存储与 title（/sessions 相关）

- `SessionManager::list_sessions()` 现成（crates/yourai-core/src/session.rs:79），返回 `Vec<SessionMeta>`；SQLite 查询已按 `updated_at DESC` 排序（storage/sqlite.rs 约 298 行）。`SessionMeta` 含 `title: Option<String>`、`model`、`created_at/updated_at`、`parent_session_id`（session.rs:31-41）。
- **session title 持久化已实现**：第一条用户输入触发 `view.note_title()`，后台任务 `load_session` → `save_session` 写回（ui.rs 约 556 行起）。`/sessions` 列表有真实数据，不是新需求。
- TUI 访问入口：`h.sessions`（ui.rs 已有用法）。
- `delete_session()`、`fork_session()` 也已存在（session.rs 80-85），v1 不暴露。

### 无现成数据（不要假装有）

- **全仓库无价格/成本数据**（`grep -ri 'price\|cost'` 0 命中）。`$ 成本` 显示属 P2，需先在配置加 pricing（见 §8）。
- 无 git branch 获取逻辑。

### 测试基线

- render.rs 内 3 个测试断言了侧栏文案（"SESSION OVERVIEW"、"32.0K / 128.0K · 25.0%"、"8 calls"、"47.5 tok/s"、`assert!(v.sidebar)`、footer 主题按钮点击），实施时全部改写。
- `apps/yourai-tui/tests/smoke.py`：`wait_for(b'TODO')`、`wait_for(b'16.0K')`（侧栏 context 文案）、剪贴板 `'Conversation'` 等断言与当前工作树**可能已经脱节**。**动手前先跑一遍 `cargo test -p yourai-tui` 和 `python3 apps/yourai-tui/tests/smoke.py` 确认基线**，红绿状态记录下来再开始。

---

## 2. 目标布局总览

### 2.1 宽屏（≥80 列且存在 todos）：对话 + 右侧 Todo 面板

```
 ✦ Fix parser off-by-one                                3 new · ^End follows╮─ Todo · 2/5 · ^T ─
 ──────────────────────────────────────────────────────────────────────────│ [✓] Inspect parser
                                                                            │ [•] Fix boundary
  Review the parser boundary and preserve existing behavior.                │     conditions
  ┃ ✦ Thinking · 4.4K · 先梳理 render.rs 的覆盖层结构…                       │ [ ] Run regression
  ┃ ≡ read   src/parser.rs · 214 lines · 0s                                │     tests
  ┃ ❯ shell  cargo test --workspace · ✓ · 3s                               │
  ┃   test parser::bounds ... ok                                           │
  ┃   test render::cards ... ok                                            │
  ┃ ✎ edit   src/parser.rs  +24 −8 · 0.3s                                  │
  ┃    214  - fn parse(&mut self, buf: &str) -> Token {                    │
  ┃    214  + fn parse(&mut self, buf: &[u8]) -> Token {                   │
  ┃    ⋯ 还有 21 处改动 · ^O 展开                                          │
  ┃ ❯ shell  cargo clippy · ⠋ 2s                                           │
  ┃   Compiling yourai-tui v0.1.0 …                                        │

  All boundary tests pass; fixing the clippy warning next.

 ⠋ Running shell: cargo clippy · 4m33s · Esc 打断                          │
 ╭─ Steer current turn · Esc interrupts ─────────── ^T todo · ^B stats · F1 help ─╮
 │▌                                                                               │
 ╰────────────────────────────────────────────────────────────────────────────────╯
 ● Running shell · 4m33s                          ▓▓▓░░░░░░░ 22% · ↑891K ↓12K · 2/5 · YOLO
```

布局结构（沿用现有 `cols`/`rows` 代数，改动小）：

- `panel_visible = v.todo_panel && !v.todos.is_empty() && area.width >= 80`；
  为真时横向 `[Min(50), Length(panel_w)]`，**`panel_w = (area.width / 3).clamp(32, 52)`**（用户明确要求比旧侧栏 29 列更宽；80 列时面板 32 / 对话 48，160 列时面板 52）；
  为假时全宽（现状）。
- 纵向 rows：`[transcript, ask, activity?(0/1), narrow_todo_dock?(0/1), input, footer]`，footer 只在左列（现状即如此）。
- Todo 面板为**整列**（右侧），顶部左边框分隔，样式照搬旧 sidebar 的 `Borders::LEFT` + `PANEL` 底。
- 输入框、ask、覆盖层、toast 一律 `BorderType::Rounded`（╭╮╰╯，见 §4.6）。

### 2.2 无 todos 或窄屏（<80 列）

- 无 todos：单列全宽，无其他变化。
- 窄屏且有 todos：右侧栏放不下，回退为输入框上方**单行 dock**（占 1 行，插在 ask 行与 input 行之间）：

```
 ─ TODO 2/5 · ► Fix boundary conditions · ^T ────────────────────────
```

`►` 后为第一个未完成任务，`elide()` 截断。这是唯一保留的"对话区占行"，宽屏不出现。

---

## 3. 状态栏规范（footer，左列最底行）

```
[空闲]  coding-model · ready                                     ▓▓░░░░░░ 22% · ↑891K ↓12K · 2/5 · ask
[忙碌]  ● Running edit · 4m33s · 2 queued                       ▓▓░░░░░░ 22% · ↑891K ↓12K · 2/5 · YOLO
[限流]  ● Rate limited · retry 2/5 in 12s                       ▓▓░░░░░░ 22% · 429×2 · ↑891K ↓12K · 2/5 · YOLO
```

| 段 | 内容 | 数据源 | 颜色 |
|---|---|---|---|
| 左（空闲） | `{model} · {state}` | 见下方"模型名来源"；`SessionStatus` | TEXT |
| 左（忙碌） | `● {activity} · {elapsed}`，保留现有 pulse 动画 | `activity()`；elapsed 用 `Xm Ys` 格式（≥60s 时），现为裸 `{n}s` | pulse |
| 左（追加） | ` · {n} queued`（n>0 才显示） | `h.host.queued()`（现藏在 sidebar 文案里） | MUTED |
| 左（限流） | `cooldown_seconds > 0` 时 activity 替换为 `Rate limited · {n}s` | `v.model_metrics.requests.cooldown_seconds` | YELLOW |
| 右 1 | `▓▓░░ 22%` context（bar 10 格 + 整数百分比） | `v.context_usage`（estimated / context_window） | <70% GREEN，70–85% YELLOW，≥85% RED |
| 右 2 | `↑{in} ↓{out}`（`tokens()` 用现有 K 格式化） | `v.usage` | MUTED；宽度紧张最先裁掉 |
| 右 3 | `{done}/{total}` todo | `v.todos` | MUTED |
| 右 4 | `YOLO` / `trusted` / `ask` | `Metadata.yolo / trusted_shell` | YOLO 恒 ACCENT |
| 告警 | `429×{n}`（n>0 插在右 2 前） | `requests.rate_limited` | RED，0 不占位 |

**模型名来源**：新增 `View.model_label: String`（启动时 = `Metadata.model`，`/models` 切换后更新）。footer、仪表盘统一改读 `view.model_label`；`Metadata.model` 字段保留仅用于启动时初始化。

**裁截顺序**（宽度不足时从右 2 开始）：tokens 段 → todo 段 → 左活动文本 elide；右 1（ctx）、右 4（权限）与模型名不裁。

**context 数据刷新**：删除 `ui.rs` 约 545 行的 `if view.sidebar &&` 前置条件（保留 1s 节流）。这是拆侧栏后状态栏存活的前提。

**删除**：footer 右侧 `[F1 /] [^B] [dark]` 按钮排、`FooterAction` 枚举、`footer_hits`。

---

## 4. 组件规范

### 4.1 Todo 侧边面板（对应用户确认的样式）

参考样式（注意复选框标记与续行对齐）：

```
 ─ Todo · 2/5 · ^T ──────────────
 [✓] SSH 到 orb 拷贝 TUI 源码到
     本地以便阅读
 [✓] 阅读关键代码：render.rs /
     state.rs / theme.rs / ui.rs
 [•] 输出基于代码现状的落地设计
     文档（不改代码）
 [ ] 实现 /models 切换
```

- **显示条件**：`v.todo_panel && !todos.is_empty() && width >= 80`；不满足时按 §2.2 回退。`v.todo_panel` 默认 `true`（由 `View.todos_expanded` 改名而来，语义反转：true=显示面板）。
- **渲染**：面板 `Block` 用 `Borders::LEFT` + `PANEL` 底；首行标题 `─ Todo · {done}/{total} · ^T ─`（ACCENT）。
- **条目**：保持**原始顺序**（不再像旧 `draw_todos` 那样按 completed 排序）。标记三态：
  - `[✓]` completed：标记 GREEN，文本 MUTED；
  - `[•]` current = 第一个 `!completed`：标记 ACCENT，文本 TEXT；
  - `[ ]` pending：标记 MUTED，文本 TEXT。
  文本按面板内宽 wrap（复用 `wrap()`），续行缩进 4 列与首行文本对齐。条目间不空行。
- **滚动**：条目超面板高度时用现有 `v.todo_scroll`（鼠标滚轮区域 `todo_area`、Alt-PgUp/PgDn 既有绑定继续生效）。
- **点击**：标题区点击 = 效果同 `^T`（`todo_hit` 现有字段）；点击条目 v1 无行为。
- **选区复制**：`Renderer.sidebar: Option<Rect>` 字段改名为 `panel`，`begin_selection` 的区域判断把 sidebar rect 换成 Todo 面板 rect。

### 4.2 ^B 仪表盘覆盖层

- 触发：`Ctrl-B` 或 `/status`；Esc / 再次 ^B 关闭（挂在与 `view.help` 关闭相同的早期分支）。
- 布局：居中 `Clear` 矩形，宽 `min(64, area.width-4)`，高按内容+2，上限 `area.height-2`；内容静态快照（数据本来 1s 刷新，overlay 打开期间自然随帧更新）。
- 文案与分组（整合旧 sidebar 全部内容，修正黑话）：

```
╭─ Session ─────────────────────────────────────────── esc / ^B ─╮
│ {model_label} · {theme}                                        │
│ {cwd，~ 缩写}                                                  │
│ session {id 前8位} · {calls} calls · {运行时长}                │
│─ Context ──────────────────────────────────────────────────────│
│ {used} / {window} ({pct}%)  ▓▓▓▓░░░░░░░░░░                     │
│ {remaining} input remaining of {budget} budget · reserve {n}   │
│─ Requests · this run ──────────────────────────────────────────│
│ {calls} calls · {n} in last 60s · {active} active · {n} cancelled
│ {failed} failed（红，>0） · {n} HTTP 429（红，>0）              │
│ {x} tok/s last response · cache hit {pct}% ({a}/{b} samples)   │
│─ Tokens · session ─────────────────────────────────────────────│
│ {in} in · {out} out · {total} total · {n} responses            │
│─ Permissions ──────────────────────────────────────────────────│
│ YOLO · approvals skipped / Trusted local execution / Ask…      │
╰────────────────────────────────────────────────────────────────╯
```

- 旧文案修正映射：`3/60s` → `3 in last 60s`；`53.3 tok/s · last E2E` → `53.3 tok/s last response`（该指标含 TTFT，见 model/metrics.rs:19 注释，不能叫 E2E）；`Cache hit 93.1% · known` → `cache hit 93.1% (27/28 samples)`；session id 只显示前 8 位。
- `journal_errors > 0` 时新增一行红字 `Request log write failed`（现 sidebar 有，搬过来）。
- `queued > 0` 不进仪表盘（已在状态栏左槽）。

### 4.3 时间线与工具卡片（tool card）

目标：**每个工具的效果默认直接可见**（可观测性），但又不是展开成一大坨 JSON。默认形态 = **卡片**：1 行标题 + ≤5 行类型化预览；完整内容仍由点击标题 / `^O` 展开（沿用现有 `expanded` 折叠机制，卡片态取代现在的"折叠单行"）。

**卡片通用结构**：左竖条 `┃` 为强调条，颜色 = 状态；标题行与预览行共享同一竖条前缀。

```
┃ ✎ edit   src/ui/state.rs  +24 −8 · 0.3s
┃    334  - ## 8. 测试计划                ← 整行 DIFF_DEL_BG 浅红底
┃    334  + ## 9. 测试计划                ← 整行 DIFF_ADD_BG 浅绿底
┃    ⋯ 还有 21 处改动 · ^O 展开
```

- **标题行**：`┃ {glyph} {tool} {summary} {徽标} · {时长|状态}`
  - glyph 按工具类型（通用 Unicode，不用 Nerd Font 私用区）：edit/write `✎`、shell `❯`、read `≡`、grep/search `⌕`、web 类 `↗`、其他 `✦`。
  - 徽标按类型：edit/write = `+a −b`；shell = `exit 0` 或 `exit N`；read = `N lines`；search = `N hits`。
  - 状态：running = braille spinner（`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`，与 §4.5 共用同一 40ms tick 帧序号）+ ACCENT；done = `✓` GREEN；failed = `✗` RED；interrupted = `■` MUTED。
- **类型化预览**（≤5 行，`┃  ` 前缀缩进）：

| 工具 | 预览内容 |
|---|---|
| edit / write（含 diff） | 解析输出中的 `+`/`-` 行：行号槽居左（MUTED 右对齐；**diff 自带行号则渲染，没有就只渲染 +/-，先确认 harness diff 格式**）；`-` 行 RED 字 + `DIFF_DEL_BG` 整行底色，`+` 行 GREEN 字 + `DIFF_ADD_BG` 整行底色，上下文行 MUTED；超 5 行尾部 `⋯ 还有 N 处改动` |
| shell · running | `progress` 末尾 3 行（TOOL 色），每帧跟随滚动——运行过程实时可见 |
| shell · done | exit 0：输出最后 3 行（无输出显示 `no output` MUTED）；failed：前 5 行 RED + `^O 展开完整输出`（取代现行"失败只露一个 `!`"） |
| read | 内容前 2 行（MUTED） |
| grep/search/webfetch | 前 4 条命中/结果 |
| 其他 | `pretty()` 前 4 行 |

- **thinking 卡片**：`┃ ✦ Thinking · 4.4K · {首行非空文本截断}`；running 时 ACCENT + spinner + ITALIC，结束后 MUTED + ITALIC；展开显示全文（现状）。
- **空行规则**：卡片之间不空行（密度）；助手文字块前后各 1 空行（段落感）。
- **数据注意（不要改结构）**：`ToolView.output` 已是 `tool_output()` 的格式化文本（edit = `"path\ndiff…"`，shell = `"exit …\nstdout···stderr"`）。渲染层按 `t.name` 解析这些文本行；v1 不改 `ToolView` 字段、不动 state.rs。行内样式参照用户确认的 GUI 风格截图（行号槽 + 整行底色）。

### 4.4 输入区与提示归位

- 标题左：空闲 ` Message `；忙碌 ` Steer current turn · Esc interrupts `（补 Esc 提示；现在只有 `Message · steer current turn`）。
- 标题右（右对齐 title，不占行）：` ^T todo · ^B stats · F1 help `。
- **动态高度**：`input_height = (editor_lines + 2).clamp(3, (area.height * 3 / 10).clamp(3, 14))`，上限从现在的固定 8 行改为屏高 30%（最高 14 行）；空闲/忙碌同规则。现状代码已有 `(lines+2).clamp(3,8)` 雏形（render.rs 约 156 行），本次只调整上限公式；行内滚动（top offset）保持现状。
- 审批/提问面板（ask）现状良好，不动。

### 4.5 运行指示条（呼吸灯，解决"流程像死了"的感知问题）

问题：thinking/工具卡片都出现在 transcript 最底部，用户向上滚动或内容停顿时，界面毫无"还在跑"的信号。

方案：在 **transcript 与输入框之间固定一行** 运行指示条（忙碌时占 1 行，空闲时消失、空间还给 transcript）：

```
 ⠋ Thinking · Kimi-K3 正在思考 · 12s · Esc 打断
 ⠹ Running shell: cargo clippy · 4m33s · Esc 打断
 ⠦ Rate limited · 12s 后重试 (2/5) · Esc 打断
```

- 内容 = braille spinner + `activity()` 文案（含运行中工具名与 summary 截断）+ elapsed + 操作提示，一行放下，超出 elide。
- 颜色 = ACCENT，整套行使用现有 `pulse_color()` 呼吸（MUTED↔ACCENT 平滑插值）——**spinner 帧与卡片 spinner 共用同一 40ms tick 帧序号**，视觉同步。
- 布局：`rows = [Min(1) transcript, ask_height, activity_height(0|1), narrow_dock(0|1), input_height, 1 footer]`。
- 与 footer 的关系：footer 是"会话级环境信息"（常驻），此行是"输入焦点旁的生命体征"（仅忙碌）；footer 忙碌文案保留，二者不互斥。
- 用户向上滚动查看历史时，此行始终贴在输入框上方——"系统还在运行"的信号不依赖 transcript 位置。

### 4.6 视觉风格（去 ASCII 化，GUI 感）

- **圆角**：输入框、ask、help/stats/models/sessions 覆盖层、toast、command 菜单、launcher 全部 `BorderType::Rounded`（╭╮╰╯）。Todo 面板和 transcript 只用单边细分隔线（`Borders::LEFT` / `TOP`），天然无角、保持利落。
- **弃用 ASCII 大字分隔线**：不再出现 `─ TODO ────` 这类横线文案；标题一律由 Block title 承载（如 Todo 面板 title ` Todo · 2/5 `、窄屏 dock 用圆角 Block title）。
- **强调结构**：工具卡片用左竖条 `┃`（状态色）而非整框包裹——接近 Slack/Notion 的 callout 观感，比 box 轻。
- **选中态统一**：pickers / command 菜单 / launcher 的当前行 = `PANEL` 背景 + `►` 前缀 + ACCENT（去掉"箭头+反转"混用）。
- **图标与 spinner**：只用通用 Unicode（`✓ ✗ ✎ ❯ ≡ ⌕ ↗ ✦ ► ○ ● ■ ⠋`），**禁止 Nerd Font 私用区字形**（保证任意终端可显示）。
- **滚动条（P1 尾部/P2）**：transcript 右缘用 ratatui `Scrollbar`（track `▐` MUTED、thumb `█` BORDER→ACCENT），提供位置感，GUI 感收益大、实现成本低。

### 4.7 主题体系（IDE 级调色板 + 跟随系统）

**语义槽从 10 个扩到 14 个**（新增 `FAINT`、`YELLOW`、`DIFF_ADD_BG`、`DIFF_DEL_BG`）：

| 槽 | 用途 |
|---|---|
| BG / PANEL / TEXT / MUTED | 现状不变 |
| FAINT（新） | 行号槽、第三级文本、`⋯ 还有 N 处` 等；派生公式 `mix(MUTED, BG, 0.45)` |
| YELLOW（新） | 告警语义独立：Notice Warning、ctx 70–85% 档、`429×n` chip、限流活动槽（这些目前都借用 ACCENT，见 §3 相应条目把 ACCENT 读作 YELLOW） |
| ACCENT | 品牌主色：焦点边框、spinner/呼吸灯、picker 当前项、YOLO（保持"权限=品牌强调色"的记忆点） |
| GREEN / RED / BLUE / CYAN(=现 TOOL) / BORDER | 现状语义；tool body 用 CYAN |
| DIFF_ADD_BG / DIFF_DEL_BG（新） | diff 整行底色；派生公式 `mix(GREEN/RED, BG, 0.85)`（明暗主题同式） |

**主题清单**（参考主流 IDE 官方调色板；以下 hex 为实施锚点，允许以官方板微调 ±2）：

| theme | BG | PANEL | TEXT | MUTED | ACCENT | GREEN | RED | YELLOW | BLUE | CYAN | BORDER |
|---|---|---|---|---|---|---|---|---|---|---|---|
| dark（VS Code Dark+） | 1E1E1E | 252526 | D4D4D4 | 858585 | 569CD6 | 6A9955 | F14C4C | D7BA7D | 569CD6 | 4EC9B0 | 3C3C3C |
| light（GitHub Light） | FFFFFF | F6F8FA | 1F2328 | 59636E | 8250DF | 1A7F37 | D1242F | 9A6700 | 0969DA | 0A7EA4 | D1D9E0 |
| one-dark（Atom） | 282C34 | 21252B | ABB2BF | 5C6370 | C678DD | 98C379 | E06C75 | E5C07B | 61AFEF | 56B6C2 | 3B4048 |
| monokai | 272822 | 3E3D32 | F8F8F2 | 75715E | FD971F | A6E22E | F92672 | E6DB74 | 66D9EF | A1EFE4 | 49483E |
| solarized-dark | 002B36 | 073642 | 93A1A1 | 586E75 | 268BD2 | 859900 | DC322F | B58900 | 268BD2 | 2AA198 | 0E4B5B |
| solarized-light | FDF6E3 | EEE8D5 | 657B83 | 93A1A1 | 268BD2 | 859900 | DC322F | B58900 | 268BD2 | 2AA198 | D8CFAF |
| nord（现有保留） | 2E3440 | 3B4252 | ECEFF4 | A5B1C5 | 88C0D0 | A3BE8C | BF616A | EBCB8B | 81A1C1 | 8FBCBB | 4C566A |
| dracula（现有保留） | 282A36 | 343746 | F8F8F2 | A2A8C5 | BD93F9 | 50FA7B | FF5555 | F1FA8C | 8BE9FD | 62B6CB | 626785 |
| catppuccin（Mocha） | 1E1E2E | 181825 | CDD6F4 | A6ADC8 | CBA6F7 | A6E3A1 | F38BA8 | F9E2AF | 89B4FA | 94E2D5 | 45475A |
| tokyo-night | 1A1B26 | 1F2335 | C0CAF5 | 565F89 | 7AA2F7 | 9ECE6A | F7768E | E0AF68 | 7DCFFF | 73DACA | 3B4261 |
| gruvbox | 282828 | 32302F | EBDDB2 | 928374 | FE8019 | B8BB26 | FB4934 | FABD2F | 83A598 | 8EC07C | 504945 |

dark/light 替换现有自定义值（nord/dracula 保留现值、补新槽）；允许槽间同值（如 solarized 的 ACCENT=BLUE），见 §11 映射规则。

**theme.rs 重构（低侵入路线）**：
- `Theme` 枚举扩为：`System, Dark, Light, OneDark, Monokai, SolarizedDark, SolarizedLight, Nord, Dracula, Catppuccin, TokyoNight, Gruvbox`；`pub const ALL: &[Theme]`；`name()` 用 kebab-case。
- 颜色载体从"常量+索引数组"改为 `pub struct Palette { bg, panel, ..., diff_del_bg }` + `Theme::palette() -> Palette`。渲染层继续使用**现有常量（= Dark 基准槽值）**，`apply()` 改为"Dark palette 各槽值 → 目标 palette 同槽值"的查表换色——缓存 markdown 行总是以基准色存储，映射无歧义（这就是现状机制，只扩槽不换架构）。
- **跟随系统**：`Theme::System` 为默认值（config `theme = "system"`）；启动时一次性解析，优先级：macOS `defaults read -g AppleInterfaceStyle`（含 Dark → Dark，否则 Light）→ Linux `gsettings get org.gnome.desktop.interface color-scheme`（含 dark → Dark）→ `COLORFGBG` 环境变量（尾部 `;N`，N≤6 视为深色背景 → Dark，≥8 → Light）→ 回退 Dark。运行时监听系统切换属 v2。
- 检测逻辑做成可注入依赖的纯函数单测：`fn detect_system_theme(cmd: impl Fn(&str, &[&str]) -> Option<String>, env: impl Fn(&str) -> Option<String>) -> Theme`。
- `/theme` 无参、未知参数、`--help` 的候选列表随 `Theme::ALL` 更新；P2 加 `/theme` picker 覆盖层（与 models/sessions picker 同构，选中即换、实时预览）。

### 4.8 Welcome 界面（neofetch 风双栏）

现状 `welcome()`（render.rs 约 548 行）是小机器人 + 三行提示，信息量为零。改为 neofetch 式**左图右信息**双栏，仅在 timeline 为空时渲染（触发条件不变）：

```
 __   __                _    ___
 \ \ / /__  _   _ _ __ / \  |_ _|
  \ V / _ \| | | | '__/ _ \  | |    yourai · your coding companion
   | | (_) | |_| | | / ___ \ | |    ──────────────────────
   |_|\___/ \__,_|_|/_/   \_\___|   model      kimi-k3 (gateway) ▓▓░░ 22%
    ~~~~~~~~ 渐变：BLUE → CYAN       workspace  ~/YourAI-Harness
    按列 RGB 插值                    session    d3f40178 · new
                                     theme      system (dark)
                                     context    300K window · 32K reserve

                                     ✦ What would you like to build?
                                     /help · ^T todos · ^B stats · /models · /sessions
```

- **Logo = figlet "YourAI" 艺术字 + 横向颜色渐变**（用户明确不要机器人/图标，要字标+渐变）：
  - 字形：**figlet `standard` 字体生成 "YourAI"**（6 行 × 32 列，即上图）；宽度充裕（`inner.width >= 78`）时升级为 `ansi_shadow` 字体（7 行 × 46 列，方块字配渐变观感最佳）。实施时用 pyfiglet/figlet 生成一次后**内嵌为字符串常量**，不引入运行时依赖。
  - 渐变：**左→右按列在 `palette().blue → palette().cyan` 间做 RGB 线性插值**（`t = col / (logo_width - 1)`），只有字形字符格子上色、空格不上色。11 套主题的 blue/cyan 都已配好（§4.7 表），渐变随主题自动变。
  - 插值 helper 抽成 `theme::lerp_color(a: Color, b: Color, t: f32) -> Color`，与现有 `pulse_color()` 共用（pulse 内部就是同一插值逻辑，顺手统一）。
  - 约束：只含 figlet 常规 ASCII/方块字形（`╗╔` 等在 Monaco/iTerm2 实测可显示），禁止 Nerd Font 私用区。
- **布局**：整块在 transcript 区水平居中；左栏 logo、其右 4 列间距、右栏信息（与 logo 垂直居中对齐）。总宽 = logo宽 + 4 + 信息栏（约 38）。高度不够（`inner.height < 12`）或宽度不足（`< 56`）时回退到现有的 3 行极简版（保留现状窄版，其 "YourAI" 标题行加 ACCENT→CYAN 同款渐变）。
- **信息右栏**（neofetch 式 `label      value`，label MUTED 右对齐/左对齐均可，value TEXT；动态数据全部现成）：
  - model：`Metadata.model`；已知窗口时尾部追加 ctx 进度条（与状态栏同源）
  - workspace：`m.cwd`（`~` 缩写）
  - session：短 ID 8 位 + `new`（无 title 时）/ title
  - theme：`view.theme.name()`（system 时显示解析结果如 `system (dark)`）
  - context：`{window} window · {reserve} reserve`（未知则 `window unknown`）
  - 分隔后一行号召语 + 一行快捷键速览（就是旧 welcome 那两句的升级版，含新增的 `/models`、`/sessions`）
- 用户发了第一条消息后 timeline 非空，welcome 自然消失（现状逻辑 `v.items().is_empty()`）。

---

## 5. `/models` 模型切换功能

### 5.1 交互

- `/models`（无参数）：打开模型选择器覆盖层。复用 `help()` 覆盖层模式；打开时键盘路由：↑↓ 移动、`Enter` 应用、`Esc` 关闭（在命令菜单分支之前拦截）。列表居中弹层，宽 `min(56, area.width-4)`。
- 列表内容 = 启动时从配置预计算的候选（见 5.3），当前使用中的条目标 `●`（ACCENT），其余 `○`；提供方分组用空行隔开（可选美化，非必须）。条目显示 `{provider}/{model}`，variant 条目显示 `{provider}/{model} · {variant}`。
- `/models provider/model` 或 `/models provider/model variant`：跳过选择器直接切换（与 `--model`/`--variant` CLI 语义一致）。
- 切换反馈：成功 → notice `Model switched to {label}`；失败（未知 provider、缺密钥、variant 不存在等 `resolve()` 错误）→ error notice，保持原模型。
- 仅空闲时允许切换；运行中的 turn 或 compaction 拒绝切换且不修改配置，模型与 context 在同一个 operation gate 下更新。
- 状态栏、仪表盘、`Metadata` 初始化处的模型显示统一改用 `View.model_label`（见 §3）。

选择器示意：

```
╭─ Models ─────────────────────────────── ↑↓ · Enter · Esc ─╮
│ ● gateway/kimi-k3                                         │
│ ○ gateway/kimi-k3 · short                                 │
│ ○ gateway/glm-4.6                                         │
│ ○ ollama/qwen3:32b                                        │
╰───────────────────────────────────────────────────────────╯
```

### 5.2 状态与 UI 改动

- `View` 新增：`model_label: String`、`model_picker: Option<usize>`（Some = 打开中选中的下标）。
- 候选列表在启动时构建一次，随 `ui::run` 传入（见 5.3）；picker 打开期间上下键只调 `model_picker` 下标，渲染读该下标高亮。
- commands.rs 增加 `/models` 条目（描述 `Switch model (picker); /models provider/model [variant]`），`argument` 标记按现状约定设置。
- `help()` 覆盖层与 `main.rs` 的 usage 字符串补充 `/models` 与 `^B stats`、`^T todo` 新语义；`docs/tui.md` 操作表同步。

### 5.3 配置与进程内流转（TUI 侧）

- 新增结构（建议放 `apps/yourai-tui/src/config.rs` 或新 `src/models.rs`）：

```rust
pub struct ModelChoice {
    pub id: String,              // "provider/model"
    pub variant: Option<String>, // 变体名；无变体条目为 None
    pub label: String,           // 显示用："provider/model" 或 "provider/model · variant"
}
pub fn model_choices(config: &Config) -> Vec<ModelChoice>;
// 遍历 config.provider × models × variants；variants 中值含 "disabled": true 的跳过；
// 每个 model 始终生成一个无 variant 的默认条目，另有 N 个 variant 条目。
```

- 切换器先克隆 `Config`，在候选配置上解析 model/variant。失败不修改当前配置或标签。
- 成功调用 `Harness::switch_model(model, candidate.context.clone()).await` 后再提交候选配置及显示标签。
- 保存结构化 `selected_variant`；`/sessions` 重建时同时解析模型、variant、上下文策略和 provider 连接设置。
- 不持久化运行时模型选择：重启后以配置/CLI 为准。

### 5.4 harness 侧改造（crates/yourai-harness）

`Harness::switch_model` 在 session operation gate 下验证并恢复新的 context manager，更新会话模型元数据后发布模型与 context。

- 忙碌或关闭中的 session 拒绝切换，避免模型与上下文窗口跨回合不一致。
- 新模型必须复用原 `MeteredModel` 的共享 budget，包括已使用调用额度和当前限流/计量策略。
- 既有 hook/subagent 执行器继续使用原模型，不跟随主模型切换。
- 集成测试覆盖切换后的 provider、共享调用上限、历史保留及上下文窗口/输出预留更新。

### 5.5 TUI 测试

- 选择器打开/上下移动/Esc 关闭不崩溃；Enter 后 `view.model_label` 更新、出现成功 notice（用最小 Config 构造，resolve 走本地校验，不发网络请求：选不存在的 provider/variant 断言错误 notice）。
- `/models bad/name` 直参错误路径。
- smoke.py 不强制覆盖切换（P2 可做：fake HTTP server 下 `/models` 切换后发消息，断言第二次请求 model 字段变化）。

---

## 6. `/sessions` 会话列表与启动选择器

### 6.1 交互（与 models picker 同构）

- `/sessions` 打开覆盖层；**顶部一行搜索框，键入即时过滤**（对 title / id / model 做大小写不敏感子串匹配）；`↑↓`/`Ctrl-P/N` 移动；`Enter` 切换到选中会话；`Esc` 关闭。
- 行格式：选中行前缀 `►`，当前会话行内 `●`（ACCENT）；时间用相对格式（`2h ago` / `3d ago`，超过 30 天用 `YYYY-MM-DD`）；`title` 为空显示 `Untitled session`。
- **默认过滤掉 `parent_session_id.is_some()` 的子会话**——subagent 内部会话不是用户会话（fork 关系目前也从该字段产生，实施时确认上游语义）。消息数没有现成字段，v1 不显示。

```
╭─ Sessions ──────────────────────── type to filter · ↑↓ · Enter switch · Esc ─╮
│ fix                                                                          │
│ ► ● Fix parser off-by-one           d3f40178 · kimi-k3 · 2h ago              │
│   ○ Fix TUI sidebar layout          9a1b2c3d · glm-4.6 · 3d ago              │
│   ○ Untitled session                5e6f7a8b · kimi-k3 · 2w ago              │
╰──────────────────────────────────────────────────────────────────────────────╯
```

- 切换守卫（v1 保守）：**当前 turn 进行中或有待答 Ask 时拒绝切换**，notice `Wait for the current turn, or press Esc to cancel it first`。
- v2 可选：`Ctrl-D` 删除选中会话（`delete_session()` 现成）；fork 暂不暴露。

### 6.2 启动选择器（`--resume` 不带 ID）

- CLI 解析：`--resume` 后**无值或下一个参数以 `--` 开头** → 进入选择器；`--resume <ID>` 保留现状直接恢复。`--help` 与 usage 文案改为 `[--resume [SESSION_ID]]`。
- 实现位置：`Harness::open` **之前**的独立轻量 alternate-screen 循环，新文件 `apps/yourai-tui/src/launcher.rs`（约 150-200 行），只需 `SessionCatalog::new(&config.session_dir)` + `list_sessions()`，不依赖 harness。
  - 流程：`Config::load`（及 `--check-config` 等早退分支）之后、`Harness::open` 之前：picker 模式 → `launcher::pick(&config) -> Result<Option<SessionId>, Error>`：`Some(id)` → `hc.resume = Some(id)` 正常启动；`None`（Esc）→ 全新会话继续。
  - 键位：字符输入 = 过滤、Backspace 删除、`↑↓`/`Ctrl-P/N` 移动、`Enter` 选择、`Esc` = 新建会话、`Ctrl-Q` = 直接退出进程。
  - 空数据提示：`No sessions yet — Esc to start fresh`。
- 搜索逻辑抽纯函数 `filter_sessions(items: &[SessionMeta], query: &str) -> Vec<usize>`（大小写不敏感，命中 title/id/model），launcher 与 `/sessions` 共用，便于单测。

### 6.3 运行时切换会话（/sessions 的 Enter）

`ui::run` 的结构性改动，按此拆分：

1. 启动时保留一份 `HarnessConfig` 模板（`resume` 字段可换）；把「`Harness::open` → `restore_history` → spawn driver」抽成事件循环内可重建的单元。
2. 切换序列（在事件循环内顺序执行）：
   a. 守卫：`view.active || !view.asks_empty()` → 拒绝并 notice；
   b. `cancel` 旧 driver、`h.host.interrupt()`、`harness.close().await`（若返回 pending 输入非空 → notice 说明丢弃数量）；
   c. 以 `resume = Some(id)` 重新 `Harness::open`；`View::default()` 重建（保留 theme 与 model_label）；`restore_history()`；spawn 新 driver；
   d. `view.title = meta.title.clone()`（若存在）。
3. **若切换链路一时无法落地，允许先合并「列表+搜索+只读」版**（Enter 时 notice `Restart with --resume {id} to switch`），后续 PR 补切换；但优先直接做完整切换。

### 6.4 数据访问与状态

- 打开 `/sessions` 时调 `h.sessions.list_sessions().await` 一次性拉取，缓存进 `View.session_picker: Option<SessionPickerState { items, filter, selected }>`；过滤结果在渲染帧内即时计算，不再查库。
- render.rs 新增 `sessions_overlay()`（复用 help 覆盖层模式 + 输入行）；键盘路由排在 models picker 之后、命令菜单之前。
- `Esc` 关闭优先级（与 §7 一致）：selection → sessions picker → models picker → stats → help → 取消执行。

---

## 7. 键位与命令（before → after）

| 键/命令 | 现状 | 新行为 |
|---|---|---|
| `^B` | 切侧栏（≥110 列才生效） | 开/关仪表盘 overlay（任意宽度） |
| `^T` | 切 `todos_expanded`（dock 已死，当前无效） | 开/关 Todo 侧栏（窄屏/无 todos 时无操作） |
| `^Y` | 换主题 | 不变（footer 按钮删除后保留此键与 `/theme`） |
| `^O` / `^R` / `F6` | 展开块 / thinking / 选块 | 不变 |
| `F1` | 帮助 | 不变，文案更新 |
| `Esc` | 取消/关 help/清选区 | + 关闭覆盖层（先清选区，再依次关 sessions picker → models picker → stats → help → 取消执行） |
| `/status` | 不存在 | 新增，等价 `^B` |
| `/models` | 不存在 | 新增（§5） |
| `/sessions` | 不存在 | 新增（§6） |

`state.rs` 字段重命名：`View.sidebar` → `View.stats: bool`（默认 false）；`View.todos_expanded` → `View.todo_panel: bool`（默认 true，注意语义反转，迁移所有读写点）。

---

## 8. 逐文件改动清单

| 文件 | 改动 | 估计规模 |
|---|---|---|
| `apps/yourai-tui/src/ui/render.rs` | 删 `sidebar()` → 拆成 `todo_panel()` + `stats_overlay()` + `model_picker()` + `sessions_overlay()`；footer 两段式；**工具卡片渲染器**（类型化预览 + diff 行号槽/整行底色 + spinner）；**运行指示条**；圆角化 + Block title 归位；窄屏 TODO dock；输入框动态高度公式；elapsed `Xm Ys`；`ctx_pressure()`；`help()` 文案；（P2）transcript 滚动条 | ~700 行 |
| `apps/yourai-tui/src/ui/theme.rs` | 语义槽 10→14（FAINT/YELLOW/DIFF_ADD_BG/DIFF_DEL_BG）；**11 套 IDE 调色板**（§4.7 表）；`Palette` struct 重构 + `apply()` 查表换色；`Theme::System` + `detect_system_theme()` | ~200 行 |
| `apps/yourai-tui/src/ui/state.rs` | 字段重命名（sidebar→stats、todos_expanded→todo_panel）；新增 `model_label`、`model_picker`、`session_picker`；`activity()` 增加限流分支 | ~50 行 |
| `apps/yourai-tui/src/ui.rs` | `^B/^T` 新语义；picker 键盘路由；**删除 context 刷新的 sidebar 前置条件**；Esc 关 overlay 顺序；`ui::run` 签名增加 `Arc<Mutex<Config>>`（或 switcher）与 `Vec<ModelChoice>`；删 `FooterAction` 分发；会话切换重建流程 | ~120 行 |
| `apps/yourai-tui/src/ui/commands.rs` | `/status`、`/models`、`/sessions` 条目 | ~10 行 |
| `apps/yourai-tui/src/config.rs`（或新 `models.rs`） | `ModelChoice` + `model_choices()`（遍历 provider/models/variants，过滤 disabled） | ~50 行 |
| `apps/yourai-tui/src/launcher.rs`（新文件） | 启动会话选择器（独立 TUI 循环 + `filter_sessions()` 纯函数） | ~200 行 |
| `apps/yourai-tui/src/main.rs` | 构建 choices、config 共享所有权、`ui::run` 新参数；`--resume` 可选值解析 + launcher 调用点；`Theme::System` 启动解析调用；usage 字符串更新 | ~50 行 |
| `crates/yourai-harness/src/default_loop/mod.rs` + `runtime/mod.rs` | 模型句柄可换（RwLock）；`SessionHost::set_model()`（含 context 预算更新） | ~40 行 |
| `apps/yourai-tui/tests/smoke.py` | 断言对齐新 UI（见 §9） | ~20 行 |
| `docs/tui.md` | 操作表新增 `^T`/`^B`/`/status`/`/models`/`/sessions` 行，删除侧栏描述，`--resume` 说明改可选 | ~15 行 |
| 测试（render.rs / 新增） | 见 §9 | ~250 行 |

---

## 9. 测试计划

实施前先跑基线：`cargo test -p yourai-tui`、`python3 apps/yourai-tui/tests/smoke.py`，记录哪些已红（工作树有半成品，**不要假设基线全绿**）。

render.rs 测试改写点（对照现有断言）：

- `palettes_context_and_toolbar_are_consistent`：删侧栏文案断言；改为：footer 含 `25%`、模型名、`2/5`；设 `v.stats = true` 后重绘断言 `32.0K / 128.0K` 与 `8 calls` 在 overlay 内；footer 主题按钮点击用例删除（按钮没了）。
- `welcome_footer_and_live_activity`：`assert!(v.sidebar)` → 新默认 `assert!(!v.stats)`；断言 `^B stats` 提示存在；`2 queued inputs` 移到 footer 左槽断言。
- `layouts_fit_narrow_and_wide_terminals`：补断言——120 列含 `[•]` Todo 面板项，50 列含单行 dock。
- **工具卡片**：构造 edit 输出断言 `+`/`-` 行底色 = `DIFF_ADD_BG/DIFF_DEL_BG`、预览上限 5 行 + `还有 N 处`；shell running 显示 progress 尾部 3 行、failed 露前 5 行；thinking 卡片含首行摘要与 K 数。
- **运行指示条**：busy 时存在（含 spinner 帧字符与 activity 文案）、idle 时消失；`elapsed` 格式化（构造 `since = now - 290s` → `4m50s`）。
- ctx 颜色阈值（60%/85%/100% 三档 fg 断言，档位色分别为 GREEN/YELLOW/RED）。
- **主题**：遍历 `Theme::ALL` 逐主题渲染不 panic 且 footer 各段存在；`Palette` 完整性（见 §11.14 的 Dark 基准互异断言）；对比度守卫——`(TEXT|ACCENT|GREEN|RED|YELLOW|BLUE) vs (BG|PANEL)` 相对亮度比 ≥ 3.0（公式：`(L1+0.05)/(L2+0.05)`，超出允许列豁免表）；`detect_system_theme` 四分支注入单测。
- **welcome**：宽屏断言 `yourai · your coding companion` 与 model 行存在、窄屏回退极简版；替换现有 `.--------.` 机器人断言。
- `/models`：`model_choices()` 单测（含 variant 与 disabled 过滤）；picker 渲染/键盘路由/错误路径测试（§5.5）。
- `/sessions`：`filter_sessions` 单测（§6.5）；overlay 渲染与拒忙守卫测试。
- harness：`set_model` 集成测试（§5.4.4）。

smoke.py 对齐建议：

- `wait_for(b'TODO')` → `wait_for(b'Todo')`（面板标题为 `Todo · n/m`）；
- `wait_for(b'16.0K')`（原侧栏 context 文案）→ 改为发送 `\x02`（^B）后 `wait_for(b'16.0K')`（仪表盘内含该文案），结束再 `\x02` 关闭；
- **圆角边框 + 新 welcome 会改变首屏单元格内容**：涉及选区复制（剪贴板内容断言）与边框字符的用例必须重跑校准坐标与期望值；
- 其余断言跑通后逐项核对，勿预设。

---

## 10. 分期与验收

> **范围与顺序（对实施 agent 的要求）**：本文档的 UI 需求**默认全量实施**——P0 + P1 一次完成，P2 条目在外部依赖就绪时一并纳入（无依赖的滚动条、theme picker 直接做）。推进顺序固定为 P0 → P1 → P2；**每完成一个阶段必须达到该期验收标准（测试全绿）再进入下一阶段**，中间状态保持可编译、可运行。分期只是实施顺序与验收闸门，不是交付边界——最终交付 = 全量。

**P0（布局与视觉骨架）**
拆侧栏 → ^B 仪表盘；两段式状态栏（模型/ctx/tokens/todo/权限/queued/429）；context 刷新解耦；Todo 侧边面板（32-52 列，含窄屏 dock 回退）；**运行指示条（呼吸灯）**；**圆角化 + Block title 归位**；**输入框动态高度**（30% 上限）；**主题体系重构**（Palette struct + System/dark/light + 系统检测）+ **welcome neofetch 风重做**；输入框标题提示；footer 按钮拆除；字段重命名；render/smoke 测试对齐。
验收：120 列与 80 列终端目检 + `cargo test -p yourai-tui` 全绿 + smoke.py 全绿。

**P1（工具卡片 + 主题全量 + /models + 会话入口）**
**工具卡片渲染器**（类型化预览、diff 行号槽与整行底色、卡片 spinner、thinking 摘要卡片）——本期最高优先；其余 9 套 IDE 调色板补齐（含对比度守卫测试）；elapsed `Xm Ys`；cooldown 进活动槽；`429×n` chip；`/status` 命令；**`/models` 全链路**（选择器 + 直参 + `set_model` + 测试）；**启动选择器**（`--resume` 无值 → launcher，独立于运行时切换，可单独先交付）；**`/sessions`**（列表 + 搜索 + 运行时切换；切换链路卡顿时允许只读版先行）。
验收：卡片在 edit/shell/read 三类工具上目检达到 §4.3 样式；主题挨个过完渲染不炸；harness 假 provider 切换测试通过；`/models` 三组交互手测；launcher 三条路径手测；`/sessions` 切换后 transcript/title/状态栏全部指向新会话。

**P2（需要先铺路）**
- transcript 滚动条（Scrollbar widget）；`/theme` picker（与 models/sessions picker 同构，选中即换）；系统主题运行时监听。
- 状态栏 `$ 成本`：先给 `yourai.json` 模型配置加可选 `"pricing": {"input": $/M, "output": $/M}`，harness 记账，TUI 乘法显示；无价格源不做假显示。
- idle 左槽 git branch（读 `.git/HEAD`，注意 cwd 与性能）。
- edit 卡片在默认预览里露出更多 diff 上下文（阈值调优，目检定）。
- smoke.py 覆盖 `/models` 切换（fake server 双 model 断言）与 launcher（PTY 下 bare `--resume` + 过滤 + Enter）。
- `/sessions` 行内消息数（需扩展 list SQL 加 count）与 `Ctrl-D` 删除会话。

---

## 11. 实施注意事项（坑）

1. **`ui.rs` 约 545 行的 `if view.sidebar &&` 刷新门槛**：删侧栏后第一件事处理它，否则状态栏数据冻结且难排查。
2. **`todos_expanded` → `todo_panel` 语义反转**（true 从"展开列表"变"显示面板"），逐处核对读写；`draw_todos` 旧排序逻辑（completed 沉底）废弃，面板按原始顺序。
3. **footer 主题按钮删除**会同时移除测试中通过 `footer_hits` 点击换主题的用例——换主题路径仍有 `^Y` 与 `/theme`，测试改走键位/命令。
4. **`DefaultLoop` 内模型的使用点**不止一处（主请求、compact、hook 可能各持路径）；实施 `/models` 时全文搜索 `self.model` 确认所有消费点都经由"请求开始时克隆"的入口。
5. **`Config::resolve()` 有副作用**（写回 `self.context` 的窗口/限额）：切换模型时务必将更新后的 `config.context` 传给 `set_model`，否则 context 预算与模型不匹配。
6. Esc 的关闭顺序：selection → models picker → stats overlay → help → 取消执行，逐层短路，别互相吃掉。
7. 覆盖层渲染顺序：picker/stats 应在 toast 之后渲染？不——toast 是瞬时反馈，应保持最上层（render.rs 中 toast 现居末尾，overlay 插入其前即可）。
8. **工作树正被其他 agent 并行改动**：勘察后已发现 `drive()` 增加 `TurnLimits` 参数、`note_title()` 签名变为返回 `Option<String>` 等漂移。实施每个章节前重新定位符号，别照抄行号。
9. `--resume` 可选值解析的歧义：手动解析（无 clap）下，`--resume --yolo` 必须把 `--yolo` 视为下一个 flag 而非会话 ID——按「下一个参数以 `--` 开头或不存在 → 无值」处理，并把该行为写进 usage。
10. **`/sessions` 与 launcher 都要过滤 `parent_session_id.is_some()`**，否则 subagent 子会话会刷屏。
11. 切换会话的重构顺序：先抽「open+restore+drive」单元再换 session；**先 cancel 旧 driver 再 close**，否则旧 harness 的 `Out` 事件会继续灌进新 View。
12. **`apply()` 换色映射的源色必须只取 Dark 基准槽值、且 Dark palette 14 个槽 RGB 互不相同**（同值会造成查表歧义）；加一个 unit test 断言 Dark 基准互异。缓存 markdown 行始终以 Dark 基准存储，映射无歧义——这是低成本重构的关键前提。
13. `detect_system_theme` 全部分支失败必须静默回退 Dark：不得 panic、不得阻塞启动（`defaults`/`gsettings` 一次性调用，<50ms；Wayland/无该命令环境会失败，属正常路径）。
14. diff 行号槽：先抓一条真实 edit 工具输出确认格式（是否 unified、有无行号）再决定行号槽渲染；**没有行号就省略该槽，不伪造**。
15. 用户终端环境实测：iTerm2 + Monaco 12（无 Nerd Font）——braille spinner、`█ ▄ ▀` 方块字、圆角字符均可用；**全程禁止 Nerd Font 私用区字形**。
16. 圆角边框 + 新 welcome 会改变单元格内容：PTY 复制断言（`clipboard_file` 期望值）与涉及边框字符的用例必须重跑校准，详见 §9。
17. 不要 `git commit`；改动留在工作树，由用户决定提交节奏。
