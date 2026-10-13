# 06 · 把日志 SQL 移回存储

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`RequestControl.start/finish`、`RequestLog.write/flush`、`Attempt`。见 [control.rs](../../crates/yourai-harness/src/model/control.rs)、[request_log.rs](../../crates/yourai-harness/src/storage/request_log.rs)。

**原因**：模型准入模块知道 model_requests 表和 SQL，准入与持久化编码混在一起。

**怎么改**：由已有 RequestLog/storage 负责 SQL 和字段编码；RequestControl 只报告请求开始/结束，删除被替代的 SQL 路径。

**验收**：数据库中的成功、失败、取消、请求身份及顺序正确；flush 和故障计数保持一致；模型准入不包含 SQL 编码。

**保持**：保留有序后台写入、Drop 非阻塞入队、pacing 和 cooldown，不新增日志管理器。
