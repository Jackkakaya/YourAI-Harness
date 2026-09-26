# TUI 交互性能与本轮优化

日期：2026-09-24。范围：YourAI 的消息布局、滚动输入调度与统计刷新；不是终端模拟器的整体帧率测试。

## 发现与处理

1. 主循环以 40 ms 间隔处理输入，最多约 25 次/秒；滚轮事件会等下一次 tick。改为 16 ms，保持输入批处理和静止画面不重绘。忙碌状态动画仍由已有 100 ms 状态快照控制，不强制每个 tick 刷屏。
2. 原有条目缓存避免了重复解析，但每次流式更新仍把所有历史 Line/Span 深拷贝成一个大数组。改为共享不可变排版块和起始行索引；只复制当前视口的行，二分定位可见块及工具标题。不变历史的 Arc 复用由确定性测试保证。
3. ctx 每秒刷新在 UI 循环中构建包含历史和工具 schema 的估算请求，并等待读取用量。移至后台任务，同时最多一个刷新任务；切换会话丢弃旧任务，关闭时取消等待，不让旧会话统计进入新会话。已经开始的只读 spawn_blocking 工作不能强制中止，会自行结束，其结果不再提交给 UI。
4. 选择/复制所需的屏幕快照复用已有 Buffer 存储；仅可见工具标题更新动画和点击区域。
5. 历史行数因过程折叠缩小时，同步减小滚动偏移，避免按旧行数偏移回看位置。

## 修改前后基准

同一机器，160×48 的 ratatui TestBackend；500 轮请求/回答、1000 条目、约 8000 排版行；预热后每种情景采样 120 帧。流式情景每帧追加一小段文字并改变滚动偏移。以下是本轮 release 实测的一组结果，毫秒/帧。

| 场景 | 修改前 P50 | 修改后 P50 | 修改前 P95 | 修改后 P95 |
| --- | ---: | ---: | ---: | ---: |
| 静态历史滚动 | 0.216 | 0.217 | 0.226 | 0.227 |
| 流式输出 + 滚动 | 0.681 | 0.256 | 0.718 | 0.273 |

流式场景 P95 下降约 62%；纯静态绘制开销基本不变。纯滚动的感知改善主要来自输入处理间隔及统计工作移出 UI 路径。此测试不包含终端实际输出、窗口合成、鼠标设备、周期统计查询，不能据此宣称已经测得端到端 60 FPS 或所有卡顿消失。

调试构建样本的帧耗时明显更高，且易受并行编译干扰，不作为改善比例依据。体验测试使用 release 构建。

复现命令：

```bash
cargo test --release -p yourai-tui long_conversation_render_benchmark -- --ignored --nocapture
```

运行优化构建：

```bash
cargo run --release -p yourai-tui -- --config ./yourai.json
```

## 正确性与后续边界

93 项 TUI 测试通过，另有 1 项手动性能基准默认忽略。覆盖共享缓存、任意视口切片、缩放重排、轮次跳转、分组展开/收起、失败和编辑不被分组合并、键盘选中隐藏项、单行底栏与长草稿。工作区 Clippy 及模型切换/UI 离线 PTY smoke 通过。

性能剩余空间：正在增长的单条 Markdown 仍会重新解析；布局变更仍扫描条目索引；首次打开长会话和大幅改变窗口宽度仍需重新排版。后续应补充超长单条回复、1000 个高亮代码块、不同终端/中文输入/真实持续滚轮的端到端测量，再决定是否增量解析。当前没有把尚未测量的路径标为已解决。

## 滚轮输入与开发构建（2026-09-24）

本轮从实际运行的 debug 二进制继续排查，补充 ANSI 编码与 PTY 输入探针。修改：

- Unix 终端关闭 1003 悬停上报，保留 1002 拖选及 SGR 点击、滚轮事件。
- 每轮输入由固定 32 个改为 2ms 时间预算、最多 512 个，突发输入处理后统一绘制，避免悬停/触控板事件跨多帧排队；预算在事件之间检查，不中断正在处理的操作。
- 滚轮每事件一行，减小批量事件造成的跳跃；逐事件限制顶部偏移，反向滚动不用先抵消不可见的越界距离。PageUp/Down 保留十行。
- dev 仅优化 yourai-tui、ratatui、Unicode 宽度与分段热路径，保留调试符号和断言；普通 cargo run 同样生效。

同机 debug 基准：1000 条消息、约 8000 行、160×48。ANSI 编码到内存（含布局、差分，不含终端绘制）P50 4.062→0.420ms，P95 4.177→0.442ms，输出仍约 6297 字节/帧。此次收益主要来自开发构建热点优化，不能解释为 release 同比提升。

新增 `tests/smoke_scroll.py`：隔离临时目录与本地 mock 模型，生成 500 段长回复，经 PTY 注入滚轮事件，等待同步帧结束标记。测试旧有 debug 二进制与新版 debug：滚动 P95 27.31→9.94ms；256 个排队悬停事件后的键入回显 136.92→12.35ms。两次为端到端应用版本对比，不是仅一个变量的实验；数值包含输入调度和 PTY 输出，不包含 iTerm 的解析、合成与显示器刷新，亦不代表所有触控板手势的延迟上限。

复现：

```sh
cargo test -p yourai-tui long_conversation_render_benchmark -- --ignored --nocapture
cargo build -p yourai-tui
python3 apps/yourai-tui/tests/smoke_scroll.py
# 可传入待比较的二进制绝对路径
```

## 终端原生滚动区（2026-09-24 第二轮）

用户实测滚动仍不足 10fps。PTY 探针证实应用管线本身不是瓶颈（16ms tick 内完成、持续 62.5fps），瓶颈是**每帧输出体积**：语法高亮代码的每个 token 切换都是约 28 字节的 24-bit SGR，代码密集内容在 160×48 达 796KB/s、238×64 达 1130KB/s，慢速终端模拟器（Terminal.app、VSCode 终端、tmux 面板）解析与合成跟不上，感知帧率由模拟器决定。

修改（`terminal.rs`）：

- 新增 `ScrollBackend`：维护与 ratatui 上一帧一致的整屏 Buffer 模型；当会话区（renderer 每帧发布的 transcript 矩形）在两帧间是纯垂直位移时，改发 DECSTBM 滚动区 + `CSI S/T`，让终端搬移自己的屏幕缓冲，只重画新暴露的 1–N 行；区域外（footer、composer、侧栏）仍走逐格差分。
- 自研编码器替换 crossterm 后端的逐格输出：前景与背景色分开追踪（ratatui 0.29 任一色变化都会连发双色 SGR），同底色的 token 重着色不再重发背景；修饰符按位差分。
- 非纯位移帧（区域内有编辑、动画头、跳转）自动回退到全量差分，行为与旧路径一致；`Buffer::diff` 已剔除宽字符覆盖格，暴露行绘制沿用同一跳过规则。
- `SizedBackend` 泛型化为任意 `Backend` 的尺寸适配层；clear/resize 使模型失效并重建。

同机 debug 实测（mock 模型生成 70 段语法高亮 Rust 代码，90Hz 持续滚轮 2s）：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 160×48 持续吞吐 | 796KB/s（6.4KB/帧） | 26KB/s（0.4KB/帧） |
| 238×64 持续吞吐 | 1130KB/s（9.0KB/帧） | 33KB/s（0.5KB/帧） |

往返正确性：`tests/scroll_roundtrip.py` 经 PTY 滚动 25 行再返回，解码 ANSI 流重建屏幕网格，与滚动前逐行一致（0 行差异，导航标签行除外）。单元测试用内嵌 VT 解码器验证滚动序列与网格等价、区域外编辑禁用快捷路径、空区域不滚动、同背景不重发。

复现：

```sh
python3 apps/yourai-tui/tests/scroll_rate_code.py   # 字节/帧与帧率
python3 apps/yourai-tui/tests/scroll_roundtrip.py   # 往返屏幕一致
```

边界：跳转（Ctrl-Home/turn 跳转）与区域内的动画仍走全量差分；终端不支持 2026 同步输出时行为不变（滚动序列照常生效）。

## SSH + tmux 下的滚动（2026-09-24 第三轮）

用户实测 SSH + tmux 下 yourai 仍卡，而 opencode/codex 丝滑。本地 tmux 探针（`tests/scroll_tmux.py`，PTY 内跑真实 tmux，滚轮事件经 tmux 转发给 pane）与 opencode 对照（`tests/scroll_tmux_opencode.py`，同一 mock 模型）找到两个差距：

1. **每事件字节数不是差距**：tmux 链路下 yourai 7.4KB/事件，opencode 9.4KB/事件——tmux 对 pane 滚动（CSI T）与逐格重绘都会触发约 30 行的客户端重绘，两条路径字节同级。直连时 yourai 的原生滚动区路径（0.4KB/帧）在 tmux 后被展开，因此 `ScrollBackend` 检测 `TMUX`/`TERM` 含 tmux 时自动回退普通差分，交给 tmux 自己的增量客户端协议；`YOURAI_TUI_SCROLL=off` 可强制关闭测量。
2. **输入延迟才是差距**：yourai 事件→响应 p50 11–12ms，opencode 1.6ms。原因：yourai 主循环以 16ms tick 轮询输入，滚轮事件平均等 8ms 才被读；opencode（Ink）由 stdin 事件驱动，到达即处理。SSH 往返叠加后这个差距在每次滚动上都被放大。

修改（`ui.rs`）：

- 专职线程阻塞读 crossterm 事件送入 channel；主循环改为 `tokio::select!` 三路唤醒——输入 channel（立即绘制）、harness 输出（流式跟随不再等 tick）、16ms tick（动画与周期统计）。Ctrl-Q 之外，读线程消亡视为退出。
- 保留批处理语义：一次唤醒先取已排队事件；≥2 个（触控板动量、粘贴）再开 2ms 窗口收 burst 尾巴，单键零附加延迟。

实测（debug 构建）：

| 指标 | 之前 | 之后 | opencode 对照 |
| --- | --- | --- | --- |
| 直连滚轮→帧延迟 p50/p95 | 10.2/12.6ms | **1.45/1.94ms** | — |
| tmux 链路滚轮→帧 p50/p95 | 11–12/17–19ms | **1.8/2.4ms** | 1.6/3.9ms |
| tmux 链路字节/事件 | 7.4–7.6KB | 7.5KB（同级） | 9.4KB |
| 256 排队事件后键入回显 | 11.3ms | 6.2ms | — |

正确性：往返探针 0 行差异；launcher/models/scroll 全部 PTY smoke 通过；100 项测试通过，Clippy 无警告。tmux 探针需注意环境变量泄漏（`TMUX`/`TERM`）会让直连模式误判，探针已显式清理。

复现：

```sh
python3 apps/yourai-tui/tests/scroll_tmux.py          # tmux 链路延迟与字节
python3 apps/yourai-tui/tests/scroll_tmux_opencode.py # 对照组
```

## 滚动果冻感：帧率洪泛与缓动滚动（2026-09-25）

用户在 UURemote（远程终端，自带 tmux 定制版 mux）中实测滚动仍像果冻：内容落后于手指、再一坨坨追上来。探针定位：应用侧单事件延迟已经很低（见上轮），果冻感的来源不是单帧延迟，而是**帧数洪泛**——每个滚轮事件触发一帧重绘，触控板惯性 90–120Hz 时即 90–120fps × ~3KB/帧 ≈ 300KB/s 灌入 mux；mux 每个 pane 更新都触发一次客户端重绘，远端客户端画不过来，输出在链路里排队、滞后累积，停下后还要慢慢排空。`tests/scroll_glide.py`（TERM=tmux-256color 模拟 mux 路径，120Hz 注入 2s）基线：**90fps、287KB/s，每事件一帧，零合并**。

修改：

- **缓动滚动（`state.rs`/`render.rs`）**：滚轮/翻页只移动 `scroll_target`；屏幕显示的 `scroll` 由动画步进逼近目标——步长为剩余距离一半、上限 3 行/帧（60fps 时约 180 行/s 峰值），收尾自动减半实现缓出。大幅甩动读作惯性滑动，单格滚轮仍在一帧内到位。锚点/轮次跳转/follow 属于导航，直接写两个偏移立即落位，不参与动画。
- **动画自限速（`render.rs::step_scroll`）**：步进带时间门（直连 15ms、mux 下 33ms——mux 每帧都要客户端重绘，30fps 的行级滑动观感相同、重绘减半），触控板突发只按动画时钟出帧。
- **滚动输入不再逐事件强制重绘（`ui.rs`）**：滚轮事件不进 `inputs` 强制绘制计数，由动画步进经 FrameSnap 差分（新增 `scroll` 字段）驱动绘制；清理活动选区时例外。
- **通用帧率节流（`ui.rs`）**：所有绘制（输入、流式输出、动画、统计）之间强制 ≥12ms 间隔，错过窗口的置 `repaint_pending`，由 16ms tick 补画。拖选、连续按键、流式跟随同样受保护，慢链路不再积压秒级帧队列。
- **内容增长位移**（`render.rs`）：流式输出加长对话时同时位移 `scroll` 与 `scroll_target`（目标为 0 即"回到底部"时保持钉住），阅读历史时视口稳定、滑向底部时自然到达新底。

实测（debug 构建，`scroll_glide.py` 120Hz × 2s，mux 模拟路径 160×48）：

| 指标 | 修改前 | 修改后 |
| --- | --- | --- |
| 滚动期帧率 | 90fps（=事件率，每事件一帧） | **28fps**（mux 下 33ms 步进） |
| 应用吞吐 | 287KB/s | **98KB/s** |
| 事件结束后输出拖尾 | 队列排空（随链路无界） | **303ms**（动画缓出收敛，有界） |

单事件响应与既有回归不退化：真实 tmux 链路滚轮→帧 p50/p95 **1.5/2.3ms**（间隔事件仍立即响应，动画时钟只约束密集突发）；直连路径 54fps、21KB/s（原生滚动区 0.4KB/帧）；PTY 单事件 p50 1.07ms、256 排队事件后键入回显 5.42ms；往返探针 0 行差异；launcher/models/UI/scroll smoke 全过；100 项测试 + Clippy 通过。

边界：mux 判定沿用 `TMUX`/`TERM` 环境变量；mux 下动画 30fps 是保守选择，若快链路的本地 tmux 用户反馈偏慢，可改为按 PTY 写阻塞时长自适应。流式输出本身仍逐事件出帧（受 12ms 节流约束），未做内容侧合并。

复现：

```sh
python3 apps/yourai-tui/tests/scroll_glide.py     # 帧率/吞吐/拖尾（mux 模拟）
python3 apps/yourai-tui/tests/scroll_rate.py      # 直连路径帧率
python3 apps/yourai-tui/tests/scroll_roundtrip.py # 往返屏幕一致
```

> 上表为缓动版本的结果。用户实测后反馈"还是一样的卡"——该版本随后被推翻，见下一节：瓶颈判断有误，真正的问题是滚轮步长与 2026 同步包裹。

## 与 codex/opencode 对照定位真因（2026-09-25 第二轮）

缓动版实测无感后，改为对照排查：克隆 openai/codex 与 anomalyco/opencode 源码阅读滚轮路径，并用 `tests/scroll_mux_compare.py` 把三个应用放进**同一条真实 tmux 链路**（`mouse on` + UURemote 同款 copy-mode 滚轮绑定 + mock 模型 + 相同注入），从**客户端侧**测量。codex 需按其 Responses API schema 提供完整 `output_item.added/done` 与 `usage` 字段（无 `[DONE]` 标记），探针已内置。

三方事实：

| | codex 0.156.1（用户安装版） | opencode 1.18 | yourai-tui（当时） |
| --- | --- | --- | --- |
| 架构 | 默认 inline：对话写入终端原生 scrollback | 应用自绘视口 | 应用自绘视口 |
| 鼠标 | **不捕获**（`fullscreen_transcript` 0.156 默认 false） | 捕获（默认开） | 捕获 |
| 滚轮归属 | mux 原生 copy-mode（UURemote 调优为 1 行/tick），应用零字节 | 应用，**3 行/事件**，即时跳变 | 应用，1 行/事件 |
| mux 客户端侧 | 0 | 313KB/s、3.7KB/事件（丝滑） | 272–632KB/s、3.6–9.0KB/事件（卡） |

结论：

1. **链路吞吐从来不是瓶颈**。opencode 以 313KB/s 的客户端字节在同一条 UURemote 链路上丝滑，说明"帧洪泛"理论错误；缓动版的降帧率/降字节自然无效。
2. **滚轮步长差 3 倍才是"卡"的主观来源**：opencode/codex 每格 3 行，我们 1 行——同样的事件流下内容移动慢三倍，读作"拖不动"。缓动动画还让消费速度封顶（mux 下 90 行/s），持续惯性时显示落后目标越来越远，火上浇油。用户"还是一样的卡"（速度这个最直观变量未变）与此完全吻合。
3. **2026 同步输出包裹在 mux 内使 tmux 客户端字节放大 6 倍**：同帧数下包裹 8.97KB/事件 vs 不包裹 1.47KB/事件。tmux 收到包裹帧后放弃增量客户端协议、整块重发。此前"mux 下两条路径字节同级"的结论是在包裹开启时测得的，被包裹本身污染。opencode（Ink）不发 2026，因此从未受影响。

修改（`render.rs`/`state.rs`/`ui.rs`）：

- **滚轮每格 3 行**，即时落位（对齐 codex/opencode）；撤掉缓动动画与 `scroll_target` 全套机制（方向错误，git 历史可回溯）。
- **mux 内自动关闭 2026 包裹**（`SyncWriter` 检测 `TMUX`/`TERM`；直连保留防撕裂）。每帧仍单次 `write()`，mux 读取后按一个批次应用，原子性不受影响；`YOURAI_TUI_SYNC=on|off` 可强制指定（探针靠 `on` 的 2026l 标记计帧）。
- 保留上一轮真正有价值的部分：12ms 帧间隔 + `repaint_pending` 补画、滚轮事件不强制立即绘制（经 FrameSnap 差分驱动）、FrameSnap 增加 `scroll` 字段。

实测（同一 tmux 链路、相同 mock、~85Hz 注入 2s，客户端侧）：

| | 修改前 | 修改后 | opencode 对照 |
| --- | --- | --- | --- |
| 客户端吞吐 | 632KB/s | **131KB/s** | 313KB/s |
| 客户端字节/事件 | 8.97KB | **1.52KB** | 3.70KB |
| 滚轮步长 | 1 行 | **3 行** | 3 行 |

回归：100 项测试 + 工作区 Clippy 通过；往返探针 0 行差异；PTY 单事件 p50 1.53ms；launcher/UI/scroll smoke 全过。

遗留：流式输出仍逐事件出帧（受 12ms 节流）；codex 的 inline（终端 scrollback）模式在长会话检索、轮次跳转上体验不如自绘视口，暂不跟进；若快链路本地 tmux 用户反馈 2026 关闭后有撕裂，可按 client terminfo 的 sync 能力细化开关。

复现：

```sh
python3 apps/yourai-tui/tests/scroll_mux_compare.py yourai opencode  # 三方对照（含 codex）
```

## 探针 harness 合并与两处测量修正（2026-09-25 第三轮）

滚动修复定稿后清理探针代码：14 个脚本各自复制一份 mock 模型服务器（chat + codex Responses 两种 wire 格式）和 PTY 驱动（~70 行/份）。合并到 `tests/smoke_support.py`：

- `start_model_server(text)`（双端点 mock）、`write_yourai_config`/`write_opencode_config`
- `Probe`：直连 PTY（app 侧帧率/字节/延迟类探针）
- `MuxProbe`：真实 tmux 链（客户端侧探针；`pane(tmp, port)` 回调构建 pane 命令，可选返回额外环境变量；`mouse=True` 附 UURemote 式 copy-mode 滚轮绑定；`passthrough` 经 `-e` 转发 pane 环境变量）
- `_Tap` 读线程统一应答 `\x1b[6n`/DA1/DA2/`?u` 四种能力查询并计数

各探针瘦身为纯测量逻辑；smoke*.py 的有状态多轮 mock（工具调用/限流/usage）语义不同，保留独立。`pty_probe.py` 收敛为 VT 屏幕解析器 + 交互调试入口。

过程中修正两处测量协议错误（都曾指向"回归"假象，教训是探针也要对照验证）：

1. **计帧标记失效**：mux 内 2026 包裹默认关闭后，`scroll_glide.py`/`scroll_bytes.py` 以 `\x1b[?2026l` 计帧会数出 0。现在探针显式设 `YOURAI_TUI_SYNC=on`（包裹本身仅 16 字节/帧）。
2. **tmux 链延迟缺间隔**：重写 `scroll_tmux.py` 时丢了事件间 `sleep(0.06)`，30 个事件背靠背发射，全部撞上 12ms 帧节流门（`repaint_pending` 等下一个 16ms tick）→ 恒定 ~15ms 的"延迟回归"。恢复间隔后 p50=1.8ms；背靠背场景本身恰是节流设计的正确行为（同链路 opencode 背靠背同样 ~14ms）。
3. **列表别名 bug**：`scroll_mux_compare.py` 里 `chunks = p.tap.chunks` 本地别名绑定了 reset 前的列表对象（`reset()` 会替换列表），测量读的是 setup 阶段旧数据 → "0.07KB/事件"假象（实为 setup 字节 ÷ 事件数）。改为按属性访问后回归 1.55KB/事件。重构涉及"会被整体替换的容器"时禁止本地别名。

修正后同一组探针复测（debug 构建）：mux 链 yourai 1.55KB/事件 vs opencode 3.60KB/事件、双方 tail=0ms；无 mouse 链 yourai 2.40KB/事件 vs opencode 9.41KB/事件，单事件延迟 yourai p50 1.8ms vs opencode 2.2ms；直连原生滚动路径 0.7KB/帧、62fps@90Hz；mux 模拟路径（TERM=tmux-256color + sync=on 计帧）63fps、tail=8ms。100 项测试 + Clippy + 全部 smoke 通过。

## 去除 case-by-case 累积（2026-09-25 第四轮）

滚动定稿后清理"为修某个 bug 加一段特判"的累积代码，原则：**同一不变量只允许一个实现处**。

1. **重绘判定收敛到 FrameSnap 单一机制**（`ui.rs`）。此前"是否需要重绘"由五个并行来源投票：`drained`（流出事件数）、`inputs`（`forces_repaint` 逐事件判定）、`context_tick`（stats 任务落地时手工 diff 字段）、`repaint_pending`（帧距）与 FrameSnap 差分本身——每个都是某次"漏重绘/多重绘" bug 的事后补丁，`context_tick` 尤其典型：它存在的唯一原因是 `context_usage` 漏在 FrameSnap 之外。现在 `draw()` 读取的全部显示状态（补齐 `selection`、`context_usage`、`usage`、`model_metrics`、`model_label`、`title`、`recorded_responses`）都在 FrameSnap 里，规则只剩一条：**快照变了（或还有待画帧）就画**。`forces_repaint`、三个计数器与 stats 落地处的手工 diff 全部删除；契约写进 FrameSnap 的文档注释——`draw` 读新状态时往这里加字段，禁止在调用点加一次性标志。配套给 harness 侧 `BudgetSnapshot`/`RequestMetrics`/`ContextUsage` 补了 `PartialEq`。
2. **滚动导航算术统一**（`render.rs`）。折叠锚定、轮次跳转、reveal 三套各自手写 `len - (line ± offset) + height`，收敛为 `scroll_to(line, row)` 一个原语——"把第 line 行放到视口第 row 行"。
3. **mux 判定与测量开关移到构造期**（`terminal.rs`）。`ScrollBackend::draw` 原本每帧做 3 次环境查询（`YOURAI_TUI_SCROLL` + `TMUX` + `TERM`），改为构造时算好存字段；测试改用 `test_set_native_scroll`。

行为等价验证：100 项测试、Clippy、全部 smoke；探针复测与上轮一致（直连 62fps/0.7KB 帧、mux 模拟 62fps/tail 0ms、PTY 单事件 p50 1.57ms、mux 链 1.50KB/事件）。期间一次 3.09ms 的"延迟回归"经插桩与复测排除——是 mux 对照探针刚杀掉 tmux server 后的瞬态机器负载，教训写进探针文档：**改完核心循环后跑对照测量前，先让机器安静**。

## 结构收敛：选择器、工具输出与换行族（2026-09-26 第五轮）

滚动与重绘机制定稿后，把剩余的"同一件事三处写"收掉。逐文件核过一遍：命令分发与 theme.rs 排除（每臂/每主题是数据，不是重复），ToolDone 的 per-tool match 与 quiet_exploration 名单保留（数据形状分发与展示策略，非特判）。

1. **共享选择器 chrome**（`overlays.rs`）。models/sessions/themes 三个 picker 各自复制居中圆角框、► 光标、●/○ 当前项标记、焦点底色、可见行滚动。收敛为 `PickRow` + `pick_list()` 一个实现；sessions 空态并入 header 行。
2. **ToolDone 归位**（`state.rs` → `state/tool_output.rs`）。每工具字段抽取（diff 行数、created、stderr、url/title、结果行）从 reducer 搬到展示投影层 `ToolResult` + `tool_result(name, output)`；state.rs 的 ToolDone 臂缩为"合并 started 侧预览 + 调投影"。reducer 只剩状态迁移。
3. **换行族合一**（`cards.rs` → `markdown.rs`）。全仓曾有四个换行实现：`cards::wrap`（纯文本同前缀）、`wrap_bg`（diff 整行底色）、`markdown::wrap_spans`（带样式 span、续行缩进）、`render::wrap_todo_text`。前三个是同一 grapheme 走行算法的三份拷贝，收敛为 `wrap_spans_with_prefixes(prefix_style)` 一个实现 + 两个薄包装 `wrap_spans`/`wrap_text`；`wrap_bg` 变为 `wrap_text` + 补底色填充。`wrap_todo_text` 保留——它断行时丢弃触发换行的空白以保持 `[x]` 对齐，语义确实不同。配套 `push_wrapped()` 收掉 tool 卡里 8 处 `for line in … { lines.extend(wrap_text(…)) }` 样板。
4. **ToolStarted** 的三连 `name == "write"` 收敛为 `is_write`。

行为等价验证：100 项测试、工作区 Clippy 0 警告、全部 smoke（含 35 列窄屏 stats 与 Ctrl-O 折叠）、PTY 单事件 p50 1.57ms、mux 链 1.53KB/事件——与第四轮基线一致。

代码量（`#[cfg(test)] mod` 边界切分）：UI 模块 HEAD 9,770（6,965 生产 + 2,805 测试）→ 9,642（6,841 + 2,801），净 -128。其中滚动修复本身在 `ui.rs` 增 +94 行（SyncWriter/2026 管理 + FrameSnap 补全），其余文件合计 -218。横向仍是最小实现：opencode TUI 5,883 行 TS + opentui 框架 + 33,665 行共享 ui 包；codex TUI 263,759 行。

## 组织与去重审计（2026-09-26 第六轮）

对"组织是否要调整、有没有重复、能不能抽象"做了一轮系统审计（逐文件过一遍 + 定位重复模式），结论分三类：

**做了的（真问题）**：

1. **SyncWriter 移入 terminal.rs**（`ui.rs` → `terminal.rs`）。同步输出是终端 I/O，与 `ScrollBackend`/`Encoder`/`inside_tmux` 同属一层；此前 UI 主循环文件里混着 57 行底层写策略。纯搬迁，行为不变。
2. **ask 覆盖层渲染归位**（`render.rs` draw() 内联 73 行 → `overlays.rs::ask_overlay`）。此前唯一的例外：pickers/stats/help 都在 overlays.rs，permission/reply 面板却内联在 draw() 里。现在 draw() 只负责布局与装配，所有覆盖层渲染同处。
3. **`switch_model` 结果通知双写**：/models picker 与 /models 命令各有一份一模一样的 match+notice（17 行 ×2）→ `switch_model_notice()`。
4. **provider 前缀提取四处同型**：`model.split_once('/').map(|(p,_)| p).unwrap_or_default()` 在 ui.rs ×2、main.rs ×1、config.rs 半份 → `Config::provider_id()`。
5. **会话守卫+文案双写**：`view.active || !view.asks_empty()` + "Wait for the current turn…" ×2 → `idle_or_warn()`（/clear 的守卫更严（还查 queued/compact），语义不同，保留）。

**评估后拒绝的（抽象有成本）**：

- **`elide`/`elide_tail` 合一**：两个 17 行函数同构（grapheme 走行 + 省略号），但合并只省 6 行，调用点从 `elide_tail(x, w)` 变成 `clipped(x, w, true)`——布尔参数在 8 个调用点引入噪点，不值。
- **run()（~840 行）拆分**：键处理/命令执行/session 切换抽出需要 12 字段上下文结构体或命令 trait 注册表；事件循环本身线性、段落有注释分节。拆分是搬迁+参数爆炸，不是简化。codex 的对应循环同规模。
- **命令双源**（commands.rs 注册表供 palette、ui.rs match 执行）：注册表是数据（显示文案/参数提示），执行是行为（异构副作用，多数 async 且依赖 harness/config/任务句柄）。强行统一为 trait 分发是过度设计。

验证：100 项测试、Clippy 0、全部 smoke（含 approval 面板与 /models 切换路径——正好覆盖本轮改动）、PTY p50 1.31ms、mux 链 1.22KB/事件。

代码量：UI 模块 9,770 → **9,582**（生产 6,965 → 6,781，净 -184 vs HEAD；六轮累计 -188）。


## 状态所有权与提交契约重构（2026-09-26）

本节取代第四轮关于“FrameSnap 已包含全部显示状态”的结论。之后的独立审计用 PTY 复现菜单选择、Esc、空闲缩放、Ctrl-Home 和连续拖选漏绘；现有局部测试仍全绿。另用 VT 仿真复现局部宽度 transcript 原生滚动破坏右栏。两组回归先失败，再重构。

- 删除 FrameSnap 与 Overlay 的手工 Snapshot，改为内存 Canvas 的完整 cell/光标比较。`Renderer::prepare` 先完成布局和交互状态解析，`Presentation` 再决定终端提交。没有逐操作的强制重绘例外。
- 最小提交间隔仍为 12ms，延迟请求由独立 deadline 唤醒；周期时钟为 100ms，负责动画与后台状态。静止时会准备可见画面，但不写终端；历史 Markdown/高亮缓存继续复用。
- Navigation 集中阅读偏移、锚定、reveal 和 turn 意图；follow/手动滚动自动取消旧意图。删除 Renderer 的三个导航字段与未使用的 unseen 计数。阅读位置按稳定条目 ID 定位，取代仅依据总行数差的补偿。
- 原生滚动要求全宽区域，否则普通差分；用局部左栏、中间区域及往返测试验证侧边内容保持不动。
- CI 纳入 smoke_ui 与新增 smoke_frames，并移除脚本不存在时跳过的分支。详见 tui-review-design.md 的交互更新与提交契约。

本机 debug 构建，1000 条历史的手动 renderer benchmark：纯滚动 p50/p95 0.323/0.337ms，流式追加 0.604/0.664ms；这是本轮测量，不作为跨机器性能门槛或与旧记录严格同条件的对比。需要保留的正确性门槛是历史缓存复用、静止无输出，以及优化前后最终屏幕等价。

本轮验证完成：workspace 测试通过，其中 TUI 112 项通过、1 项手动基准默认忽略；工作区 Clippy `-D warnings`、fmt 与 5 个离线 smoke 通过。另执行手动基准、滚动往返（0 行差异）、PTY 滚动探针（p50/p95 3.11/4.63ms，排队输入 14.07ms）和滚动输出探针（65fps、约 0.6KB/帧）。上述时间仅为本机当前构建测量，不代表所有终端链路；本轮没有重测真实 tmux 客户端链路。
