# 会话存储与内存上下文

状态：SQLite 主链已实现，TUI/Harness 默认使用。此前的 JSON 历史实现已移除。

ContextManager负责上下文变更流程，TaskManager负责任务转换，SessionManager负责记录读写和事务。内部清理与摘要可合并为一次`save_context`事务；任务复用同一SQLite，单次转换或旧数据导入原子提交。算法见[ContextManager设计](./context-manager-design.md)。

## 1. 组件与调用关系

```text
TUI / API
    │
SessionHost                         队列、恢复、执行互斥、Hook、关闭
    │
DefaultLoop                         模型 → 审批/工具 → 结果 → 继续
    │
ContextManager / DefaultContext      内存上下文、清理/摘要、先保存后更新视图
    └── SessionManager              持久化接口（core）
          └── SessionCatalog        会话目录与宿主锁
                └── SqliteStore     SQL、事务、历史分页

UsageTracker → LocalUsage → 同一个 SqliteStore 的 usage_events
TaskManager → SessionManager → 同一个 SqliteStore 的 tasks
```

实现位置：

- `crates/yourai-core/src/session.rs`：会话、消息、分页查询、压缩提交接口。
- `crates/yourai-core/src/context_manager.rs`：内存上下文接口。
- `crates/yourai-harness/src/storage/sqlite.rs`：SQL表、事务、schema升级及JSON导入。
- `crates/yourai-core/src/tasks.rs`：任务工具、hook、共享任务查询与持久化协调。
- `crates/yourai-harness/src/context/`：上下文投影、压缩及提交协调。
- `crates/yourai-harness/src/storage/usage.rs`：用量记录与统计。

ContextManager 通过 SessionManager 保存消息和摘要，通过 UsageTracker 记账；没有 SQL 或连接。归档只保留身份索引，活跃消息原文用于模型请求。system 指令由宿主装配并冻结，由 ContextManager 持有并在 build_request 使用，不通过 append 保存。

## 2. SessionManager 接口

| 方法 | 职责 |
|---|---|
| create_session / load_session / save_session | 会话元数据创建、读取、更新 |
| list_sessions | 列出元数据，按 updated_at、session_id 排序 |
| delete_session | 级联删除消息、用量和任务；宿主须已关闭 |
| fork_session | 事务复制历史，新会话和新消息 ID；不复制用量、队列和工具副作用 |
| read_messages(id, MessageQuery) | active/full 过滤，按 seq 升序，after 游标分页 |
| append_messages(id, batch) | 原子追加普通消息，返回已存记录 |
| save_context(id, ContextChange) | 单事务保存清理标记及可选摘要替换 |
| read_tasks(id) | 同步恢复该会话任务，按seq、task_id排序 |
| save_tasks(id, tasks) | 同步提交整批任务并返回已保存数据；正常转换在host阻塞worker运行 |

调用方在提交前分配消息 ID，重试复用同一 ID。同 ID、同内容不重复插入；内容或所属会话不同则报冲突。seq 是会话内顺序，不是身份。

接纳后的用户记录 id 与 UserText.id 相同，原始输入写入 content_json 的可选 yourai_input 元数据；模型请求不携带这份存储元数据。它与冻结后的附件 / recall / skill 内容一起保存，用于确认丢回执的提交。同一身份的输入内容必须一致。fork 同时改写复制消息及其输入身份，其余冻结内容保持不变。

ContextChange 包含 pruned 消息 ID、可选 CompactionChange 和暂存提交进度（不写入数据库）；后者只在内存中携带来源 ID 和新摘要。首次提交要求来源仍 active；任何验证、约束或写入失败都回滚。重试核对摘要身份和内容、来源已经 compacted；调用方必须复用原候选，数据库不保存来源集合，也不提供来源集合审计证明。

不引入 tenant_id、租约、执行日志表或万能 commit 接口。会话 ID 全局唯一，授权与跨节点执行所有权由未来服务层负责。

## 3. 数据表

默认数据库：`<catalog_root>/sessions.sqlite3`。完整DDL以`sqlite.rs`的SCHEMA和启动迁移为准；当前schema为v5。

| 表 | 字段 |
|---|---|
| sessions | session_id PK、parent_session_id、title、provider、model、system_prompt、created_at、updated_at |
| messages | message_id PK、session_id FK、seq、kind、role、content_json、format_version、tool_call_id、token_count、status、created_at、tool_output_pruned_at |
| usage_events | usage_id PK、session_id FK、provider、model、source、input_tokens、output_tokens、total_tokens、cache_read_tokens、cache_creation_tokens、reasoning_tokens、usage_json、created_at |
| model_requests | request_id PK、run_id、session_id、source、model、started_at、duration_ms、turn_id、attempt、outcome、http_status |
| tasks | (session_id, task_id) PK、subject、description、owner、completed、seq；session_id FK |

约束：

- `UNIQUE(session_id, seq)`；每会话最多一个 active summary。
- tool_call_id 关联工具结果；每会话同一个调用最多一条结果。调用本身在 assistant 消息中，不另建 tool_executions。
- kind 为 message / summary，status 为 active / compacted / archived。
- role 为 user / assistant / tool；system 不作为历史消息存储。
- content_json 保存完整 genai 消息，format_version 当前为 1；运行时附加上下文增加可选的 yourai_runtime_context 元数据。该标记在请求投影前移除，用于区分真实用户请求，不依赖正文前缀猜测。
- token_count 是估算；usage_events 保存模型报告的值，未知值保持 NULL，明细不重复累加到总量。usage_json 保存完整 genai Usage，并非整个 HTTP 响应。
- usage_id 支持用量写入重试；source 区分 main / compact / hook，旧数据导入为 legacy。尚无明确来源的 provider 保持 NULL。
- 时间使用 UTC 毫秒；外键删除级联；不维护可重建的计数缓存。

没有 source_message_ids_json、compactions、session_operations、session_leases 或 tool_executions。

`tool_output_pruned_at` 是 nullable UTC 毫秒；清理后消息仍 active，content_json 不变。只有被摘要替代的消息设为 compacted。v1 → v2 启动时事务迁移，恢复和 fork 保留标记。

SQLite 使用 WAL、foreign_keys=ON、synchronous=FULL、5 秒 busy timeout。用 PRAGMA user_version 管理版本，不增加迁移表。普通异步数据库调用在阻塞工作线程执行；同一 adapter 的工作按锁串行，调用方取消等待不意味着已提交事务能够回滚。

v4→v5新增tasks表及会话顺序索引，不改既有会话和历史。任务完成不可回退；相同身份的subject、description、owner或seq冲突使整批回滚。TaskManager在数据库提交后更新唯一缓存，两个步骤由同一个owned worker完成，等待方取消不会跳过缓存发布。

## 4. 三条执行路径

```text
追加：ContextManager.append → append_messages 事务 → 更新内存
恢复：ContextManager.restore → 读取已提交记录 → 重建活跃视图/身份索引
      宿主补记未配对工具的未知结果，禁止自动重放
压缩：ContextManager.compact
        → 内部清理 → 必要时 PreCompact / 分批摘要
        → save_context（清理 + 来源归档 + 新摘要，单事务）
        → 更新内存 → PostCompact → 返回状态
```

模型视图为 system、当前摘要、保留的 active 消息；原文和归档记录仍可按 seq 分页。提交途中被取消会将内存标记为不可读，下一次写入先恢复已提交状态。摘要选区保护最新用户请求和完整工具批次，详见压缩设计。

消息提交和工具外部副作用不可能成为同一个 SQLite 事务。崩溃后的缺失结果只表示状态未知。宿主仍记录未消费输入和 interrupted 状态；不承诺跨数据库与宿主队列的 exactly-once 输入消费。

## 5. 运行与迁移

新会话直接使用 SQLite；TUI 的 session_dir 配置保持不变。

旧任务文件`<session_dir>/<session_id>/tasks.json`在TaskManager初始化时导入同一数据库：只导入数据库尚未存在的任务，整批事务提交；原文件保留为备份，之后任务转换只写SQLite。格式或任务身份不合法时明确报错，不留下部分导入。

旧目录存在 meta.json/history.json 且未导入时，启动明确报错，不静默创建空历史。先关闭会话，再执行：

```sh
cargo run -p yourai-tui -- --config /path/to/yourai.json --import-json-sessions
cargo run -p yourai-tui -- --config /path/to/yourai.json --resume SESSION_ID
```

导入在单个事务中完成，保留原始 JSON 文件；重复导入或 ID 冲突报错，不覆盖数据库。旧 boundary 转换为消息 status，旧 summary 转为摘要消息，usage 取 history.json 中的流水，不再次导入重复的 usage.json 聚合。旧时间戳转换为毫秒。

导入范围是配置 session_dir 的直属会话；旧 children 目录保留原样，如需导入其中会话，需以该目录作为 session_dir 单独执行。新建子 Agent 与父会话共用数据库并记录 parent_session_id。

host.json、配置、权限、任务、记忆、技能仍由各自模块保存。这些不是会话消息表，当前没有为了本次迁移新增业务表。

当前只支持同一会话一个宿主执行，使用 host.lock 排他。元数据列表仍是全量接口；启动协调器还会读取归档记录以建立身份检查视图，大历史场景的分页索引优化留待后续。模型调用中断时未收到的 usage 不可推断；当前压缩用量在成功提交后记录，不提供计费级的全失败调用账本。

## 6. 验证

SQLite 专项测试覆盖事务追加/重试/冲突、游标恢复、压缩选区与回滚、摘要替换、fork/delete、并发 seq、NULL 用量与去重、三表约束和旧 JSON 导入。Context 测试覆盖预览和原文分页、清理/fork/恢复、原子失败、用量记账、工具配对与长 Turn、分批调用额度、Hook、usage 基线、不确定提交恢复。

历史依据见 [2026-09-02 讨论原文](./references/context-manager-2026-09-02.md)。旧文中的 ContextManager 自持久化分工已经废止。

提交进度在开始保存前标记 Started，在 SQL 事务 commit 成功后立即标记 Committed，之后才做视图恢复。等待方取消不清除进度，也不撤销已有 worker；未知提交必须先恢复。关闭宿主通过 ContextManager.restore 等待已开始的历史写入结算，再释放会话租约。
