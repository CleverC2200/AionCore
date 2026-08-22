# GEA 接口现状索引

> 基线：AionCore `58c08cd25d7b44fdc879b6ed142b12c12b04b27e`（2026-08-21）
>
> 本文只描述当前代码，不修改或重新设计接口。发现的问题与建议单独列出，不能作为接口变更授权。

## 1. 阅读路径

GEA 相关调用按下面的顺序追踪：

1. AionUi 的 HTTP/WS adapter 或 AionCore 的 conversation runtime helper 发起本地调用。
2. AionCore 的 GEA 路由完成参数提取、用户认证上下文和 runtime conversation scope 检查。
3. GEA 数据结构定义请求、响应、回执和本地投影格式。
4. GEA 实现保存用户级登录态和会话态，并调用远端 GEA Gateway 或 Resource Catalog。
5. 全局 InteractionRequest 额外经过 AionCore 的可恢复投影，WebSocket 事件只通知客户端重新拉取快照。

对应实现入口（`AionUi/` 表示同一产品的 AionUi checkout，其余路径相对 AionCore 仓库根目录）：

| 层次 | 当前实现 |
| --- | --- |
| AionUi HTTP/WS adapter | `AionUi/packages/desktop/src/common/adapter/ipcBridge.ts`、`httpBridge.ts` |
| AionUi GEA 登录态转交 | `AionUi/packages/desktop/src/process/services/LarkAuthService.ts` |
| AionUi InteractionRequest 刷新 | `AionUi/packages/desktop/src/renderer/hooks/system/notification/useInteractionRequestSync.ts` |
| AionCore conversation runtime helper | `crates/aionui-app/src/commands/cmd_gea_stdio.rs` |
| AionCore 本地路由 | `crates/aionui-gea/src/routes.rs` |
| 共享请求/响应结构 | `crates/aionui-api-types/src/gea.rs`、`response.rs`、`websocket.rs` |
| GEA Gateway 实现 | `crates/aionui-gea/src/service.rs` |
| Resource Catalog 实现 | `crates/aionui-gea/src/service/resource_catalog.rs` |
| InteractionRequest 投影与事件 | `crates/aionui-gea/src/service/interaction_request.rs` |
| 应用级认证、CSRF 与路由装配 | `crates/aionui-app/src/router/routes.rs`、`crates/aionui-auth/src/middleware.rs`、`csrf.rs` |

## 2. 公共约定

### 2.1 响应包装

成功响应使用：

```json
{
  "success": true,
  "data": {}
}
```

无数据成功响应省略 `data`。失败响应使用：

```json
{
  "success": false,
  "error": "可读错误信息",
  "code": "STABLE_ERROR_CODE",
  "details": {
    "category": "VALIDATION",
    "retryable": false
  }
}
```

`details` 还可能包含 `retryAfterMs`、`requestId`、`traceId`、`auditId` 和脱敏后的上游错误信息。

### 2.2 认证与 CSRF

- 所有本地 GEA 路由都经过 AionCore 认证中间件。
- Local identity mode 注入本地默认用户；其他模式接受 Bearer JWT 或 `aionui-session` Cookie。
- conversation runtime helper 使用 `x-aionui-runtime-token`、`x-aionui-user-id` 和 `x-aionui-conversation-id`，并校验三者绑定关系。
- 非 Local identity mode 的 `POST`、`PUT`、`PATCH`、`DELETE` 请求默认要求 `x-csrf-token` 与 `aionui-csrf-token` Cookie 匹配。
- runtime token 请求不使用浏览器 ambient Cookie，因此免除 CSRF，但仍必须通过 runtime token 认证和会话范围校验。
- runtime helper 不能读取、写入或清除桌面进程保存的 GEA 登录态。

## 3. AionUi / runtime → AionCore

### 3.1 GEA 登录态与会话

| 方法与路径 | 请求 → 成功数据 | 当前调用入口 | AionCore 实现 | 鉴权与错误语义 | 验证状态 |
| --- | --- | --- | --- | --- | --- |
| `GET /api/gea/auth/session` | 无 → `GeaAuthSessionStatus` | `ipcBridge.ts::geaAuth.status`；`LarkAuthService.ts::syncSharedGeaSessionToBackend` | `routes.rs::auth_status` → `service.rs::auth_status` | runtime token 返回 403；无 GEA 登录态仍返回 200 状态对象；永不返回 access token | 当前代码；本地集成测试；未验真实 GEA |
| `PUT /api/gea/auth/session` | `SetGeaAuthSessionRequest` → `GeaAuthSessionStatus` | `LarkAuthService.ts::syncSharedGeaSessionToBackend` | `routes.rs::set_auth_session` → `service.rs::set_auth_session` | 400 参数或 token 格式错误；401 AionCore 未认证；403 runtime/CSRF；500 存储错误 | 当前代码；本地集成测试；未验真实 GEA |
| `DELETE /api/gea/auth/session` | 无 → 空成功响应 | `LarkAuthService.ts::logoutSharedLarkAuthSession` | `routes.rs::clear_auth_session` → `service.rs::clear_auth_session` | 401 AionCore 未认证；403 runtime/CSRF | 当前代码；本地集成测试；不调用真实 GEA |
| `POST /api/gea/conversations/{conversation_id}/session` | `CreateGeaSessionRequest` → `GeaSessionResponse` | `cmd_gea_stdio.rs::ensure_session` | `routes.rs::create_session` → `service.rs::create_session` | 400 参数错误；401 AionCore/GEA 未认证；403 runtime scope/GEA 拒绝；502 网络、业务或响应契约错误 | 当前代码；本地 mock/integration 测试；未验真实 GEA |

`SetGeaAuthSessionRequest` 接收 `accessToken` 和可选 `tenantId`。`GeaAuthSessionStatus` 只返回 `authenticated`、`reauthRequired` 和可选 `tenantId`。

`CreateGeaSessionRequest` 接收 `consumerCode` 和可选 `preparationId`。成功数据返回 `sessionId`、`conversationId`、`consumerCode` 和 `effectiveCapabilityCodes`。

### 3.2 工具与 MCP

| 方法与路径 | 请求 → 成功数据 | 当前调用入口 | AionCore 实现 | 鉴权与错误语义 | 验证状态 |
| --- | --- | --- | --- | --- | --- |
| `GET /api/gea/conversations/{conversation_id}/tools` | 无 → `GeaToolInfo[]` | `cmd_gea_stdio.rs::load_tools` | `routes.rs::list_tools` → `service.rs::list_tools` | 401 未认证；403 runtime scope；409 未建立会话；502 网络、业务或工具 Schema 错误 | 当前代码；本地 mock/integration 测试；未验真实 GEA |
| `POST /api/gea/conversations/{conversation_id}/tools/{tool_name}` | `GeaToolCallRequest` → `GeaToolCallResponse` | `cmd_gea_stdio.rs::call_tool` | `routes.rs::call_tool` → `service.rs::call_tool` | 400 arguments 非 object；401/403 认证或 scope；404 未授权工具；409 无会话；502 上游失败 | 当前代码；本地 mock/integration 测试；未验真实 GEA |
| `POST /api/gea/mcp/test` | `CreateGeaSessionRequest` → `GeaToolInfo[]` | `ipcBridge.ts::geaAuth.testMcpConnection` | `routes.rs::test_mcp_connection` → `service.rs::test_mcp_connection` | 400 参数错误；401 未认证；403 runtime/CSRF；502 临时 Session 或工具发现失败 | 当前代码；本地 mock/integration 测试；未验真实 GEA |

`GeaToolCallRequest.arguments` 必须是 JSON object 或 `null`。返回数据包含 `result` 和可选 `auditId`。

### 3.3 InteractionRequest

| 方法与路径 | 请求 → 成功数据 | 当前调用入口 | AionCore 实现 | 鉴权与错误语义 | 验证状态 |
| --- | --- | --- | --- | --- | --- |
| `GET /api/interaction-requests?status=active` | `status=active`，兼容 `pending` → `InteractionRequestList` | `ipcBridge.ts::interactionRequest.list` | `routes.rs::list_all_interaction_requests` → `service.rs::list_all_interaction_requests` | 400 不支持的 status；401 未认证；500 投影存储错误；列表可返回 partial/failed 同步状态 | 当前代码；本地 mock/integration 测试；未验真实 GEA |
| `POST /api/interaction-requests/{request_id}/actions` | `InteractionRequestActionCommand` → `InteractionRequestReceipt` | `ipcBridge.ts::interactionRequest.act` | `routes.rs::act_on_global_interaction_request` → `service.rs::act_on_global_interaction_request` | 400 命令错误；401/403 认证或 CSRF；404 不存在；409 版本、状态、恢复冲突；502 上游失败 | 当前代码；本地 mock/integration 测试；未验真实 GEA |
| `GET /api/gea/conversations/{conversation_id}/interaction-requests` | 无 → `GeaInteractionRequestSnapshot` | 当前 checkout 未发现直接 AionUi 调用 | `routes.rs::list_interaction_requests` → `service.rs::list_interaction_requests` | 401 未认证；403 runtime scope；409 无会话；502 上游快照无效 | 当前代码；本地 mock/integration 测试；未验真实 GEA |
| `POST /api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions` | `GeaInteractionRequestActionCommand` → `GeaInteractionRequestReceipt` | 当前 checkout 未发现直接 AionUi 调用 | `routes.rs::act_on_interaction_request` → `service.rs::act_on_interaction_request` | 400 命令错误；401/403 认证、scope 或动作拒绝；404 不存在；409 冲突；502 上游失败 | 当前代码；本地 mock/integration 测试；未验真实 GEA |

全局动作请求包含 `expected_version`、`idempotency_key`、`action_id` 和可选 `payload`。全局列表和回执使用 snake_case 字段；会话级 GEA 快照与动作结构使用 camelCase 字段。

WebSocket 事件：

```json
{
  "name": "interactionRequest.changed",
  "data": {
    "user_id": "system_default_user",
    "revision": "sha256-derived-revision"
  }
}
```

触发条件是当前用户的可恢复投影发生变化，或动作回执完成本地落库与原 Turn 恢复；仅轮询但投影未变化时不广播。AionCore 将事件总线消息按载荷中的 `user_id` 只投递给该用户已认证的 WebSocket 连接。

AionUi 不发送独立 subscribe 帧：共享 adapter 连接 `/ws`，`ipcBridge.ts::interactionRequest.changed` 用事件名注册本地监听器，`useInteractionRequestSync.ts` 在组件挂载时订阅、卸载时退订，并在事件或 `realtime.reconnected` 后重新请求全局列表。该事件只是失效通知，不是完整 InteractionRequest 数据流；断线期间遗漏事件由重连后的重新拉取修复。

### 3.4 客户端资源同步

| 方法与路径 | 请求 → 成功数据 | 当前调用入口 | AionCore 实现 | 鉴权与错误语义 | 验证状态 |
| --- | --- | --- | --- | --- | --- |
| `POST /api/client-resources/sync` | `SyncGeaClientResourcesRequest` → `GeaClientResourceSyncResult` | `ipcBridge.ts::clientResources.syncFromGea` | `routes.rs::sync_client_resources` → `service/resource_catalog.rs::sync_client_resources` | 400 resources 为空；401 未认证；403 runtime/CSRF；409 Artifact 校验冲突；500 本地存储错误；502 GEA 网络或响应错误 | 当前代码；本地 mock/integration 测试；未验真实 GEA |

`resources` 接受 `assistants`、`skills`、`mcps`。当前实现只处理 `skills`：请求中不包含 `skills` 时返回 completed，并把所请求类型计入 skipped；这不代表 assistants 或 mcps 已完成同步。

## 4. AionCore → GEA

以下路径来自当前 AionCore 实现。GEA 官方接口规范仍是远端契约的最终权威；本地 wiremock 测试只能证明 AionCore 对这些形状的本地处理。

当前 AionUi 与 AionCore checkout 中没有检索到 GEA 官方 OpenAPI/Swagger 文件，因此下表不能替代 GEA 官方规范。取得官方规范后，应逐条建立对应关系并保留版本信息。

| 方法与 GEA 路径 | AionCore 用途与实现 | 当前请求要点 | 当前响应校验与错误映射 | 证据状态 |
| --- | --- | --- | --- | --- |
| `POST /ai/gateway/session` | 建立 Gateway Session；`service.rs::create_session_inner` | `consumerType`、`consumerCode`、`requestId`、`conversationId`、`channel`、可选 `preparationId` | 校验 `accessDecision.allowed`、`delegationToken` 和 gateway context；业务拒绝保留上游 code/category；无效 JSON/结构映射 502 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/agent/session` | 统一 Session 路径返回 404 时的兼容回退；`service.rs::create_session_inner` | `agentCode`、`channel` | 复用 Session 响应解析与错误映射 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/mcp/proxy/list` | 查询会话授权工具；`service.rs::list_tools` | Session、conversation、delegation token | 工具名唯一；校验 name/sourceCode；净化 inputSchema；业务错误保留上游字段，网络/超时为 502 `GEA_NETWORK_ERROR` | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/mcp/proxy/call` | 调用授权工具；`service.rs::call_tool` | Session、conversation、delegation token、mcpCode、toolName、arguments | sourceCode/toolName 必须与请求一致；提取 result/auditId；业务错误保留上游字段，网络/超时为 502 `GEA_NETWORK_ERROR` | 代码实现；本地 mock 测试 |
| `GET /ai/gateway/interaction-requests` | 拉取会话 InteractionRequest；`service.rs::list_interaction_requests_unlocked` | query 包含 agentCode、sessionId、conversationId；认证头包含用户登录态和 delegation token | 解析 revision/items 并更新投影；401 会使登录态失效，SESSION 错误会清除会话；网络/超时为 502 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/interaction-requests/{requestId}/actions` | 提交动作；`service.rs::act_on_interaction_request_unlocked` | Session 信息、expectedVersion、idempotencyKey、actionId、可选 payload | 解析回执并校验 requestId；保留上游业务 code/category/retryable/追踪字段；网络/超时为 502 | 代码实现；本地 mock 测试 |
| `GET /aidata/client-resource-catalog/my` | 获取当前用户目录；`service/resource_catalog.rs::fetch_resource_catalog` | 可选 revision query；使用用户 GEA 登录态 | 校验 status、revision、tenant 和 schema version；401 使登录态失效；无效 JSON/结构为 502 | 代码实现；本地 mock 测试 |
| `GET /aidata/client-resource-catalog/skill-artifact` | 下载 Skill Markdown；`service/resource_catalog.rs::download_and_materialize_skill` | skillCode、version、`format=md` | 校验响应头、长度、digest、version 和 UTF-8；不匹配为 409；网络/超时为 502 | 代码实现；本地 mock 测试 |
| `POST /aidata/client-resource-catalog/skill-execute/report` | 上报执行结果；`service/resource_catalog.rs::report_skill_execution` | skill、version、digest、结果、耗时、大小及可选风险/错误信息 | 成功只检查 HTTP 状态；失败解析业务错误，401 使登录态失效；网络/超时为 502 | 代码实现；本地 mock 测试 |

GEA Gateway 调用使用用户级 GEA 登录凭证；conversation 相关调用还带有当前会话的 delegation token。生产 `GeaService::from_env` 的连接超时为 10 秒、单次请求总超时为 120 秒，二者均由同一 `reqwest::Client` 覆盖上述 Gateway 和 Resource Catalog 调用。发送失败（包括连接和请求超时）统一映射为可重试的 502 `GEA_NETWORK_ERROR`；上游非 2xx 或 `success=false` 尽量保留 code、category、retryable、retryAfterMs、requestId、traceId、auditId 和脱敏 details；JSON 或结构校验失败映射为 502 `GEA_INVALID_RESPONSE`。日志只记录低敏标识和状态，不应输出 token、请求体或工具结果。

## 5. Swagger 暴露方案

首批 Swagger 只覆盖本文接口，并遵守以下冻结方案：

- 文档入口只在 debug build 中注册。
- Swagger UI 和 OpenAPI JSON 仍经过现有 AionCore 认证中间件。
- Swagger UI 禁用全部 Try-it-out submit methods，避免从文档页面误触发读写调用。
- 禁用外部在线 validator，避免把接口规范发送给第三方服务。
- OpenAPI Schema 只能增加描述元数据，不能改变 serde 字段名、必填性、默认值或业务行为。
- 如 OpenAPI 工具无法无损描述当前接口，停止该项实现并记录提案，不调整接口迁就文档工具。

脱敏的本地与 GEA 出站请求样例见同目录的 `examples.http`。样例中的地址、身份和业务参数都是占位值；写请求只应在获得相应环境授权后手工执行。

### 5.1 契约漂移检查

`crates/aionui-gea/src/routes.rs` 的 OpenAPI 单测把当前确认的 10 条路径、12 个唯一 operationId，以及会话、工具、InteractionRequest、WebSocket 事件和客户端资源同步的关键 Schema 字段/枚举值写成显式基线。`crates/aionui-app/tests/gea_openapi_e2e.rs` 另外验证未认证访问被拒绝、认证后 OpenAPI JSON 可解析且 Swagger UI 可加载。

这些测试只报告差异：路由、operationId、字段名或枚举值变化会直接失败，并指出发生漂移的路径或 Schema；没有快照自动接受或更新逻辑。确认差异是有意接口变更前，不得修改测试基线。Schema 基线验证的是文档与当前 Rust/serde 契约一致，不代表真实 GEA 环境已经验收。

## 6. 已发现但不在本项目修改的问题

| 现状 | 影响 | 处理方式 |
| --- | --- | --- |
| 全局列表兼容接受 `status=pending`，校验错误信息却只写“当前只支持 status=active” | 调试时可能误解兼容范围 | Swagger 按真实行为记录两个值；接口和错误文案均不修改 |
| AionUi 的 WebSocket adapter 只声明 `revision`，服务端事件还包含 `user_id` | TypeScript 调用方看不到完整载荷类型，但当前解析允许额外字段 | 文档记录真实载荷；不调整事件或前端类型 |
| 客户端资源枚举包含 assistants 和 mcps，但实现只处理 skills | 容易把“请求可接收”误认为“能力已支持” | Swagger 与文档明确标为 skipped 行为；不新增同步能力 |
| 当前证据主要来自代码和本地 mock/integration 测试 | 不能证明远端 GEA 当前部署与本文完全一致 | 保留真实 GEA 环境验收为独立待办，不提升验证状态 |
