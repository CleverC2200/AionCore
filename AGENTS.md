# AionCore 项目约定

本文件只保存执行约束；架构解释放入 `ARCHITECTURE.md`。新增规则应改变执行行为，避免重复事故经过、命令清单和实现说明。

## 证据与验收

- 关于 Agent CLI 协议、字段、生命周期或能力的结论，必须引用当前读过的原始来源：`~/aion/protocols/samples/` 实际采样、官方适配器或生成的 schema、`~/.cargo/registry/src/` 中 ACP 库源码、CLI 自描述输出，或真实录制且通过的集成用例。没有证据时标明未验证，先采样或读取契约；不得按名称或旧知识猜行为。
- 对代码行为的结论引用亲自检查的文件位置；其他 Agent 的结论先核对证据。宣称功能不存在前，搜索并读取相关处理链；跨层缺陷沿实际数据定位首次偏离处。
- 测试必须覆盖声称已验证的事件和路径。模拟、单一路径成功和本地检查不能替代真实 Agent 或真实环境验收；比较旧实现或正式契约后再声明等价。

## 架构与安全

- 依赖沿 Foundation → Capability → Domain → Composition 向下流动；同层通过 trait 协作，禁止循环或反向依赖。修改基础层时评估影响范围。
- 领域 crate：`lib.rs` 只声明与导出；`routes.rs` 处理请求/响应转换；`service.rs` 承载业务且不依赖 axum；`state.rs` 定义共享状态。
- API 使用 `/api/` 前缀、kebab-case 资源名及 `ApiResponse<T>` / `ErrorResponse`。请求和响应类型属于 `aionui-api-types`，该 crate 不依赖 HTTP 框架。
- `aionui_common::ApiError` 只用于 API 边界；领域服务使用本 crate 错误类型，在路由映射。错误响应不得泄露内部细节。
- 新 WebSocket 事件使用 `domain.camelCaseAction` 和 `WebSocketMessage<T>`，经 `event_bus.broadcast()` 发出；旧格式不作为新增事件的范例。
- 仓储 trait 位于 `aionui-db`、以 `I` 开头，实现以 `Sqlite` 开头；模型放在 `models/`，参数类型与仓储共置；服务依赖 trait。
- 数据库变更通过顺序编号的 `NNN_descriptive_name.sql` 迁移；按现有规范使用 `IF NOT EXISTS`，不直接修改业务数据库或已交付迁移。
- `AppServices` 统一构建服务；领域 crate 只定义 RouterState，组装放在 `aionui-app` 的 `build_*_state()`。
- 新子进程使用 `aionui_runtime` 的 spawn Builder，不直接调用 `tokio::process::Command`。
- 新端点评估认证中间件；状态变更要求 CSRF 保护，敏感操作需要限流；禁止硬编码密钥。

## 实现与日志

- 使用 `rust-toolchain.toml` 固定的工具链和 Cargo manifest 声明的 edition。注释及提交标题使用英文。
- Rust 模块按单一领域职责组织；生产源码接近 1000 行时考虑拆分，测试文件不受该提示限制。
- 修改关键或难观测路径时说明现有日志是否足够；只补必要结构化日志。生产诊断使用 info/warn/error，详细高频信息使用 debug；简单重构、测试或文案不强制加日志。
- 生产日志不包含提示词、工具输入输出、文件内容、命令体、凭据或原始供应商请求/响应；确需开发诊断时使用默认关闭的开发专用开关。

## 测试与验证

- 修改业务逻辑、端点、WebSocket 或集成测试时，读取 [测试范围](docs/agents/testing.md) 的对应要求。
- `aionui-db` 关闭自动入口发现，新增集成测试文件必须注册到 `crates/aionui-db/tests/integration.rs`。合并其他测试入口前核对完整测试集合、进程和全局状态隔离。
- 失败先判定断言是否仍代表正确需求：正确则修实现；需求已明确变化才更新断言；原因不明时先定位。禁止删除失败用例或弱化断言过门禁。
- 开发优先 `just test-package <crate>` 或 `cargo test -p <crate>`，配合 `cargo clippy -p <crate> -- -D warnings` 与格式检查。共享契约、依赖、构建链或发布验收再扩大验证，保留完整 pre-push 门禁。
- 验证复用遵循全局约定；长检查保留会话标识并报告进度。
- 提交前完成受影响 crate 的测试、Clippy 和 `cargo fmt --all -- --check`。用户授权推送后使用 `just push`，不绕过迁移、lint、format、test 门禁。

## 按需资料

- 修改分层、服务组装、接口、数据库或进程运行时前，阅读 [架构说明](ARCHITECTURE.zh-CN.md) 的对应章节，并核对当前实现；无需为无关修改通读全文。
