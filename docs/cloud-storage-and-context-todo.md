# 云端存储与上下文精简 TODO

本轮范围：Agent 运行数据使用 SQLite，业务层通过 SessionStore 接口访问；工作区源码和工具操作的业务文件保留。SQLite 是当前数据库适配器，不宣称它已经支持跨机器分布式运行。

- [x] 核对 Compactor::from_snapshot：它从冻结的 ProviderSnapshot 装配当前上下文、模型、Hook、用量和会话信息，没有额外状态或执行层。保留该构造函数。
- [x] 阅读本地 OpenCode、Codex、Pi 源码，分类对比精简机制。
- [x] 将队列、运行恢复状态、记忆、技能、权限和工作区登记迁到 SQLite。
- [x] 将大工具输出迁到 SQLite，提供按 ID 的有界读取；不再返回依赖节点本地文件的路径。
- [x] 会话执行所有权改为数据库租约，拒绝过期所有者写入；删除不得与运行会话并发。
- [x] 检查所有测试和文档，验证恢复、跨连接竞争、原子提交和取消。
- [x] 在 DefaultContext 请求投影中实现旧工具正文清理，保护近期结果和未闭合批次，达到净节省阈值才应用；通过现有 Read 回看 SQLite 原始工具消息。
- [x] 会话运行环境统一命名为 SessionEnvironment / SessionManager.environment()，与 ContextManager 聊天历史区分，更新所有调用处和文档。

## 上下文精简分类

1. 输出限幅：新工具结果仅发送头尾预览，原文存储可检索。
2. 旧工具输出清理：保留调用与状态，旧输出在请求中替换为可检索引用，不调用模型。
3. 历史摘要：旧对话生成滚动摘要，保留最近消息、当前任务与完整工具批次。
4. 上下文替换：同一个运行上下文键只发送最新版本。
5. 分支摘要：切换对话分支时总结离开的分支；当前没有分支导航需求，不增加新执行类。
6. Provider 原生 compact：由模型服务返回专有压缩项目；当前 Genai 通用 ChatRequest 没有此契约，不复制 Codex 的 provider 私有协议。

旧工具清理属于请求投影，历史摘要属于 Compactor；两者不得物理删除数据库中的历史原文。

当前已经实现输出限幅、旧工具正文清理、历史摘要和上下文替换。数据库 BLOB 原文的 7 天保留期不是 context 清理策略。本轮没有新增分支摘要或 provider 原生 compact。

## 验证结果

- 工作区测试：616 通过，0 失败，2 忽略（含 doctests）。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo fmt --all -- --check`、`git diff --check`、TUI 构建通过。
- 6 组 TUI 冒烟测试通过：smoke、launcher、models、ui、frames、editor。
- 新增数据库验证覆盖并发版本竞争、租约替换与旧写者拒绝、输出隔离和幂等追加、整份输出过期、扩展跨连接恢复、旧队列与工具输出迁移、ConfigChange JSON 提案。
- 新增 7 个旧工具正文清理测试：近期窗口、工具预算、净节省和关闭配置、runtime 消息与未闭合批次保护、实际 Agent 清理后不付费摘要、恢复与 fork 回看、摘要归档后的有界读取、媒体与纯内存上下文保留。fork 原文验证还覆盖删除父会话后的独立读取。

## 源码依据

- OpenCode `78adb7ddba66bfb7d0a4ecd5e09cbe60831e1dde`：`packages/opencode/src/session/compaction.ts` 的 prune/select/process；旧工具输出有保护窗口和最低收益阈值，摘要保留近期尾部。https://github.com/anomalyco/opencode/blob/78adb7ddba66bfb7d0a4ecd5e09cbe60831e1dde/packages/opencode/src/session/compaction.ts
- Codex `6849549b49446fc3db89ecc13d6226ab971bec79`：`codex-rs/core/src/compact.rs`、`compact_remote.rs`、`compact_remote_request.rs`；本地摘要重建历史及远端 provider compact 是不同路径。https://github.com/openai/codex/blob/6849549b49446fc3db89ecc13d6226ab971bec79/codex-rs/core/src/compact.rs
- Pi `dd1c690f36de449fac9b24c8b3d113837eaf7cfb`：`packages/coding-agent/src/core/compaction/compaction.ts`、`branch-summarization.ts`；近期窗口、滚动摘要、长 turn 前缀摘要和分支摘要。https://github.com/earendil-works/pi/blob/dd1c690f36de449fac9b24c8b3d113837eaf7cfb/packages/coding-agent/docs/compaction.md

本地参考仓库只读，没有修改它们的工作区。
