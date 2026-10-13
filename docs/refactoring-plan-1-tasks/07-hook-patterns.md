# 07 · 让 Hook 拥有编译后的模式

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`RegisteredHook.if_condition`、`condition_matches`、`HttpHandler`、`wildcard_url_match`、`cached_regex`。见 [runtime.rs](../../crates/yourai-harness/src/hooks/runtime.rs)、[matcher.rs](../../crates/yourai-harness/src/hooks/matcher.rs)、[http.rs](../../crates/yourai-harness/src/hooks/http.rs)。

**原因**：固定模式仍在运行时解析/构造，编译状态依赖不会随注册项释放的全局缓存。

**怎么改**：注册/创建 handler 时编译，结果随实际对象拥有；消费者迁完再删除无必要缓存。另记录工具字段名单属于协议映射还是补丁式 case。

**验收**：匹配、无效条件、legacy 名称、once、去重、注销和执行中 handler 正确；运行时不重新解释固定模式。

**保持**：保留无效条件当前行为；不合并工具和 URL 通配，不放宽 DNS label 限制。
