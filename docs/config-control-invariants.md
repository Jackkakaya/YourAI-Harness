# 配置和会话控制约束

本次重构针对 Config 与 controller 的写入权、状态所有权和对外职责，不扩展为整个仓库的架构保证。

| 问题 | 改进与约束 |
|---|---|
| Config 字段可被外部直接赋值 | 所有配置 struct 字段私有；共享 Config 的 setter 内部加锁并立即释放，不对外返回锁守卫；配置段 getter 返回同一 Config 的访问对象，每次 getter/setter 内部加锁 |
| 为配置增加额外共享包装类型 | 删除包装，沿用 Arc<Config>；字段私有，getter/setter 操作，持久化更新由使用方处理，Config::save 只写 JSON |
| 模型、variant、effort 分散更新和构建时隐式覆盖 | select_model 一次完成选择；models().resolve() 只构建已记录选择；校验失败不修改候选 |
| CLI 与 controller 重复拼装修改规则 | CLI 与运行切换复用 select_model；运行切换只选择和构建一次，通过 set_models 和 save 提交 |
| 模型构建读取整个应用配置 | 构建函数只接收 ModelSettings，不扩展 Config implementation |
| Prompt 准备依赖整个 Config | 只接收 PromptSettings |
| pending/restart 散在 Client、Runner、submitter | 移入私有控制状态；只能通过 enqueue/finish_input/request_resume 等转换修改 |
| 当前 Harness 和输出通道有两份可变身份 | 只在控制状态中保存；Runner 通过只读查询取得引用 |
| 生命周期和事件、命令接口重叠 | 所有者管理运行寿命；Client 提交命令；Subscription 消费事件；移除 Deref |
| controller 公开会话组装与重复模型操作 | 组装归 assembly::resolve_session；模型应用 helper 私有，只经 Client 提交运行操作 |
| 任务句柄与展示快照可能成为另一份准入状态 | 句柄负责异步完成，快照由权威状态生成，均不决定操作许可 |
| 多来源保存和差异比较使配置复杂 | 删除 save_plan 与跨文件提交；init 兼容迁移，save() 只原子保存完整主 JSON |

Config 只负责 getter/setter 和 JSON 加载、保存，业务校验由使用方负责；数据定义集中在 config.rs；模型模块中的 ModelSettings 方法负责模型选择、effort、价格和客户端构建，不拥有文件存储。没有额外的共享配置包装类型。私有 state 模块承载原有控制状态，不增加 Manager、Repository、Transport 或通用状态框架。

模块通过必要的数据与操作契约协作，禁止跨模块直接修改内部字段、循环维护状态；不以复制代码追求零依赖。

## 外部调用迁移

- 沿用 Arc<Config>；不增加共享配置包装类型。
- 锁在 Config 内部。models()/execution()/prompt() 返回同一 Config 的嵌套访问对象，不复制配置、不持有锁。provider/model/variant 按 ID 定位；setter 直接写共享数据，缺失或已删除的索引项返回错误。Config 没有 snapshot 或 Clone。
- 普通持久化修改直接调用 setter 后 Config::save；保存失败时保留内存修改。没有通用更新闭包接口。业务校验在需要它的操作中执行。
- 模型处理先调用 models().select_model 返回新的 ModelSettings，成功后通过 Config::set_models 写入，再由 models().resolve 构建客户端。provider、压缩模型也作为普通模型数据通过 setter 写入，Config 不检查业务规则。
- SessionController::new 返回 (owner, subscription)；命令经 owner.client()，事件经 subscription。
- controller::prepare_session 改为 assembly::resolve_session，函数入口冻结同一份配置数据，统一用于主模型、压缩模型和会话参数。
- 直接 serde 反序列化仅接受分类 JSON；旧平铺配置通过 Config::init 迁移。

## 验证

编译失败示例覆盖直接字段写入和通过 getter 写入。集成测试覆盖快照隔离、模型选择失败不部分修改、显式校验与保存分离、普通保存失败保留本地修改、真实客户端模型更新、旧会话退休时新输入、客户端断开与关闭。测试通过接口验证行为，不扫描源码字符串。

所有七组数据保存到同一个 JSON；旧 Prompt/Hook 文件只在迁移初始化时读取并保留为备份。保存先完整序列化，再原子替换主文件；普通保存失败保留内存修改；模型和安全切换保存失败恢复对应分组，保持运行资源。分类 setter 成对，运行操作用 change_*，内部运行效果用 apply_*。

运行切换构建完成后检查模型配置和上下文策略是否仍与构建依据一致；变化则拒绝提交，保持旧运行资源。提交只修改模型分组，保留主题等无关分组的并发更新。controller 不持有第二份权威配置，候选只用于本次构建。
