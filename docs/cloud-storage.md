# Agent 数据库存储

当前实现使用 SqliteStore，schema v6。core 依赖 SessionStore，不依赖 SQL 或节点本地状态文件。SQLite 是现阶段适配器；替换为远程数据库时必须实现相同的原子性、版本比较和所有权检查，不要求模拟目录或文件锁。

## 数据与对象

| 数据 | 数据库位置 | 使用者 |
|---|---|---|
| 会话、消息、任务、用量、请求诊断 | sessions / messages / tasks / usage_events / model_requests | DefaultContext、TaskManager、LocalUsage、ModelBudget |
| 队列、active/interrupted 输入、事件、cwd、watch、关闭状态 | session_state 的 runtime 文档 | SessionManager |
| 记忆、技能、权限、运行配置、worktree 登记 | session_state 的 memory / skills / permissions / config / worktrees 文档 | LocalMemory、LocalSkills、PolicySecurity、Workspace |
| 执行所有权 | session_owners | SessionLease、SessionManager |
| 完整工具输出 | tool_outputs 分块 BLOB | ToolOutputStore、Shell、Read |

SessionState 只是带 revision 的数据，不是新的执行类。Table<T> 对 memory/skills 共用 JSON 文档读写，读取数据库当前值，写入比较原 revision，拒绝覆盖并发变化；不再持有文件缓存。

## 真实执行路径

```text
Harness.prepare_session
  → SessionStore.claim_session
      → 返回绑定所有者的存储实例
  → 使用该实例装配 DefaultContext、工具、扩展和 Agent
  → SessionManager.open_owned
      → 读取 runtime 文档
      → 原子保存队列/恢复状态
      → 启动租约续期

Tool.exec → Shell/具体工具 → ToolOutputStore
  → SessionStore.append_output（按唯一 ID、字节 offset 保存）

Read(path="tool-output://UUID", byte_offset=0, byte_limit=51200)
  → ToolOutputStore.read → SessionStore.read_output
```

Model 的自动维护与手动操作均调用 Compactor.exec。摘要提交通过 DefaultContext.save_summary → SessionStore.save_summary，将来源消息标为 compacted 并保存摘要；原文仍在 messages 中。SQLite 的拥有者检查、原子业务操作及 COMMIT 在同一个事务中。提交确认由后台数据库 worker 在 COMMIT 成功后设置，即使等待者取消也不会把未提交当成已提交。

## 所有权与并发

SessionLease 使用 UUID 所有者，默认 60 秒租约；SessionManager 每 10 秒续期。租约创建和抢占在数据库事务中完成，只有过期租约允许替换。所有绑定所有者的 SQLite 操作都在事务中检查租约；旧实例不可写回新实例的队列、消息、任务或输出。释放使用 owner 条件，旧实例释放不会删除新实例的租约。

未绑定所有者的 SqliteStore 是管理端能力，用于列出、创建、删除、迁移和管理数据。Agent 运行依赖必须使用 claim_session 返回的实例，不能自行安装管理端连接。删除与租约检查在同一个事务中；活跃租约存在时拒绝删除。

session_state 用 revision 做比较并交换；同一旧版本的两个写者最多一个成功。队列确认成功后才发布内存状态或发送 Steer。写失败时原记录仍在数据库；未确认的写入不能被当成新消息重发。

## 工具原文

工具原文保存为有序分块 BLOB。ID 与 offset 定义块身份，重试相同块不重复插入，内容冲突或不连续 offset 被拒绝。读取最多 1 MiB，并按会话筛选；模型请求只包含有界预览、执行状态和 output_refs。Read 使用字节游标 next_byte_offset；业务文件继续使用行游标 next_offset。

旧工具正文清理在 DefaultContext 请求投影中完成，不调用模型、不改写消息。`tool-output://message/SEQ` 引用直接读取本会话原始工具消息，使用同一 Read 字节分页入口；被摘要移出活动上下文后仍可读取，不需要再复制一份工具结果。详细保护窗口和收益阈值见 context-manager-design.md。

fork 在同一个事务中复制消息和工具输出 BLOB，保留输出 ID；子会话从自己的数据读取，即使父会话删除也不失去已复制的原文。

保留时间默认 7 天。清理以整个输出 ID 为单位，只有最后一个块也过期才删除，不删除部分仍活跃的输出。后台清理只持弱引用，租约失效后退出。

ConfigChange 直接带 config 提案，不再制造 config.candidate.json。没有临时文件时 file_path 为空；Hook 在配置提交前检查具体 JSON。

## 升级及范围

SessionCatalog 只负责数据库启动、业务工作区位置和旧版本数据导入。恢复在取得租约后，将旧 host/memory/skills/permissions/config/worktrees JSON 导入缺失的文档键；已存在的数据库值不被旧文件覆盖，原文件只作为未修改的备份。tasks 的导入标记也在数据库。旧的完整会话 JSON 仍通过显式 --import-json-sessions 导入。旧 managed tool-output 文件按引用分块导入数据库，历史记录保留原内容，发送给模型时将旧路径转换成稳定 ID；缺失或已过期的原文件不会伪造内容。

源码、用户指定的文件、Git worktree、指令/技能来源文件和模型附件属于工具业务输入，不迁成数据库文档。部署配置也可以是配置文件；它不承载运行队列或恢复状态。

这一步移除了 Agent 状态对共享目录和 OS 文件锁的依赖，没有实现一个分布式服务。未来远程数据库适配器需要数据库侧时间和所有权检查；跨节点的工作区/附件可达性、进程调度、远程事件传递和租户授权属于服务部署，不能把 SQLite 文件放进共享目录就声称完成分布式运行。
