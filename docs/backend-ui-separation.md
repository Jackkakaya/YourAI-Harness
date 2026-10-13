# 后端控制，UI 展示与操作

当前实现是进程内共享后端代码，不是独立网络 Server。会话配置仍由 SessionManager 唯一持有。

## 职责

| Module | 拥有的行为 |
|---|---|
| core SessionManager | 输入队列、单轮执行、配置、持久化结算、子会话和关闭 |
| harness SessionController / 私有 Runner | 独立后台循环、提交顺序、失败后续跑、操作互斥、压缩、模型切换、创建/恢复/切换、退休会话清理、统计刷新 |
| harness SessionClient | 可克隆命令入口与最新快照；释放不取消执行，不拥有 worker |
| harness config | 读取启动设置、解析模型与 provider、校验选项、处理 effort 继承与显式选择 |
| harness TaskTool / SubagentTool | 工具 JSON schema、输入解析、结果编码；调用 core 领域行为 |
| TUI Session Controller | 把用户操作交给后端；把结果、历史和快照映射到 View |
| TUI View / Renderer / Presentation | 草稿、选择、overlay、历史显示、布局与帧提交 |

TUI 的会话层没有 driver、submitter、compact JoinHandle，没有执行重启规则，也不判断领域操作能否发生。后端拒绝操作时，TUI 显示原因。关闭是一个操作，关闭顺序和结算在后端完成。

TUI 的 poll 只消费数据。后台循环由 Tokio 任务独立驱动，UI 暂停消费事件不影响执行完成、切换完成或旧会话关闭。View 的 active/asks 只用于显示，不再成为后端操作的授权依据。

UI 仍有必要的显示控制，例如 Esc 关闭 overlay、保持草稿、忽略已经关闭的选择器的迟到结果。这些不会启动执行、选择持久化顺序或决定会话生命周期。

## 其他 UI 如何使用

```rust,ignore
let mut backend = SessionController::new(harness, config, options, limits);
backend.submit(In::user_text("任务"));
backend.interrupt();
backend.compact()?;
backend.model(model_id, variant, effort)?;
backend.switch(Some(session_id))?;

// 不调用这两个读取入口，后端也会继续推进。
let snapshot = backend.snapshot();
while let Some(event) = backend.recv().await {
    // Web、桌面或终端 UI 自行展示相同的领域数据。
}
backend.close().await?;
```

SessionEvent 表达输出、完成、列表、删除、模型选择和打开会话的结果；SessionHistory 与 SessionSnapshot 是数据。没有为不同 UI 引入 Transport、UI trait 或通用事件总线。

## 数据与协议

- 模型配置解析从 TUI config.rs 移到 harness；TUI 不再直接依赖 genai。
- ModelSelection 是共享选择结果；TUI 的 ModelInfo 只是导入此类型，不重复定义字段。
- 后端一次读取历史与用量，TUI 只投影数据，不自行分页、装配或清理候选会话。
- TaskManager 在同一锁内读取版本与列表，并在同一锁内发布更新。TUI 不再经 Harness 获取任务对象并协调 version/list。
- TaskManager 与 Subagent 不再实现工具协议；具体 Adapter 放在 harness，Hook、事务及资源结算保留在 core。
- 已解析 Hook Value 直接进入协议解释，删除 Value → 字符串 → Value 的重复编解码。

## 验证与限制

最终验证：631 项 workspace 测试、两组离线 PTY smoke、fmt、Clippy 与 diff 检查全部通过。

后端测试独立验证无 UI 消费时的执行和操作完成、操作期间输入/回复拒绝、失败切换保持原会话、禁止删除当前会话、旧会话退休期间新会话正常工作以及任务版本查询。原来的 TUI 人工 JoinHandle 注入测试由真实后端行为测试替代；显示、草稿和选择器回归测试保留。

终端 smoke 发现最初的空快照会覆盖恢复出的标题，已修复：显示层保留恢复数据，直到后端发布第一次有效刷新，并添加对应回归测试。

独立进程、网络连接、事件恢复游标及重连仍未实现。多个独立事件订阅已支持。SessionController 是后端运行所有者，close/Drop 结束执行；SessionClient 是可克隆命令与快照句柄，其 Drop 不影响执行。客户端可通过 subscribe() 创建独立 SessionSubscription；所有者保留自己的订阅用于进程内 TUI。新订阅只接收未来事件，不自动回放历史。

```rust,ignore
let backend = SessionController::new(harness, config, options, limits);
let client = backend.client();
client.submit(In::user_text("任务"));
drop(client); // 后端仍执行；Server 将持续持有 backend。
let reconnected = backend.client();
let latest = reconnected.snapshot();
backend.close().await?; // 存活的客户端不会阻碍关闭。
```

关闭开始立即拒绝新操作，关闭输入接收器并排空已接受输入；因此不会等待所有客户端释放发送端。所有者 Drop 用于异常退出清理，显式 close 用于等待关闭结果。类型清单见 current-type-inventory.md；Server 后续工作见 server-plan.md。


## 独立订阅

```rust,ignore
let client = backend.client();
let mut first = client.subscribe()?;
let mut second = client.subscribe()?;
while let Some(event) = first.recv().await {
    match event {
        SessionEvent::Lagged { skipped } => {
            // 增量历史缺了 skipped 条事件。最新状态可读 client.snapshot()；
            // 完整历史恢复尚未实现，不能继续假装展示完整。
        }
        event => { /* 投影到这个客户端 */ }
    }
}
```

广播缓冲最多保留 1024 条事件；慢订阅者不会阻塞执行。落后时 recv/try_recv 返回 Lagged 提示，再继续读取保留的事件。Drop 只退订当前消费者。显式关闭后，存活的命令客户端不会维持事件出口；订阅排空后 recv 返回 None。列表/删除事件错误在对外数据中编码为 String，内部操作仍保留 YourAiError。

这里限制的是订阅事件条数，不是整个运行时的内存；单个事件大小、内部输出桥接与命令队列尚无总字节限额。网络协议需要明确恢复游标和快照对应位置，不能把当前订阅读取位置当作可重连游标。
