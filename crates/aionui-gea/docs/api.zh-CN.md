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

对应实现入口：

| 层次 | 当前实现 |
| --- | --- |
| AionUi HTTP/WS adapter | `packages/desktop/src/common/adapter/` |
| AionUi GEA 登录态转交 | `packages/desktop/src/process/services/LarkAuthService.ts` |
| AionCore conversation runtime helper | `aionui-app` 的 GEA stdio command |
| AionCore 本地路由 | `aionui-gea` 的 routes 模块 |
| 共享请求/响应结构 | `aionui-api-types` 的 GEA 模块 |
| GEA Gateway 实现 | `aionui-gea` 的 service 模块 |
| Resource Catalog 实现 | `aionui-gea` 的 resource catalog 模块 |
| 应用级认证、CSRF 与路由装配 | `aionui-app` router 与 `aionui-auth` middleware |

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

| 方法与路径 | 请求 | 成功数据 | 当前调用方 | 关键约束 |
| --- | --- | --- | --- | --- |
| `GET /api/gea/auth/session` | 无 | `GeaAuthSessionStatus` | AionUi Renderer、桌面登录态实现 | runtime token 被拒绝；不返回 access token |
| `PUT /api/gea/auth/session` | `SetGeaAuthSessionRequest` | `GeaAuthSessionStatus` | 桌面登录态实现 | runtime token 被拒绝；写请求受 CSRF 保护 |
| `DELETE /api/gea/auth/session` | 无 | 空成功响应 | 桌面登录态实现 | runtime token 被拒绝；写请求受 CSRF 保护 |
| `POST /api/gea/conversations/{conversation_id}/session` | `CreateGeaSessionRequest` | `GeaSessionResponse` | conversation runtime helper | runtime token 必须与 path conversation 匹配 |

`SetGeaAuthSessionRequest` 接收 `accessToken` 和可选 `tenantId`。`GeaAuthSessionStatus` 只返回 `authenticated`、`reauthRequired` 和可选 `tenantId`。

`CreateGeaSessionRequest` 接收 `consumerCode` 和可选 `preparationId`。成功数据返回 `sessionId`、`conversationId`、`consumerCode` 和 `effectiveCapabilityCodes`。

### 3.2 工具与 MCP

| 方法与路径 | 请求 | 成功数据 | 当前调用方 | 关键约束 |
| --- | --- | --- | --- | --- |
| `GET /api/gea/conversations/{conversation_id}/tools` | 无 | `GeaToolInfo[]` | conversation runtime helper | 需要已建立会话；runtime conversation 必须匹配 |
| `POST /api/gea/conversations/{conversation_id}/tools/{tool_name}` | `GeaToolCallRequest` | `GeaToolCallResponse` | conversation runtime helper | 工具必须来自当前会话授权列表；写请求受相应认证约束 |
| `POST /api/gea/mcp/test` | `CreateGeaSessionRequest` | `GeaToolInfo[]` | AionUi Renderer | 建立一次临时会话、拉取工具后清理；不是 Resource Catalog 的 MCP 物化 |

`GeaToolCallRequest.arguments` 必须是 JSON object 或 `null`。返回数据包含 `result` 和可选 `auditId`。

### 3.3 InteractionRequest

| 方法与路径 | 请求 | 成功数据 | 当前调用方 | 关键约束 |
| --- | --- | --- | --- | --- |
| `GET /api/interaction-requests?status=active` | `status=active`，也接受兼容值 `pending` | `InteractionRequestList` | AionUi Renderer 使用 `status=pending` | 返回当前用户的本地可恢复投影，并在拉取时尝试同步 GEA |
| `POST /api/interaction-requests/{request_id}/actions` | `InteractionRequestActionCommand` | `InteractionRequestReceipt` | AionUi Renderer | 按 request owner 和幂等键处理；写请求受 CSRF 保护 |
| `GET /api/gea/conversations/{conversation_id}/interaction-requests` | 无 | `GeaInteractionRequestSnapshot` | 当前代码未发现 AionUi Renderer 直接调用 | 返回指定会话的 GEA 快照；runtime conversation 必须匹配 |
| `POST /api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions` | `GeaInteractionRequestActionCommand` | `GeaInteractionRequestReceipt` | 当前代码未发现 AionUi Renderer 直接调用 | 直接在指定 GEA 会话上提交动作；runtime conversation 必须匹配 |

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

该事件是用户级失效通知。AionUi 收到事件后重新请求全局列表；它不是完整 InteractionRequest 数据流。

### 3.4 客户端资源同步

| 方法与路径 | 请求 | 成功数据 | 当前调用方 | 关键约束 |
| --- | --- | --- | --- | --- |
| `POST /api/client-resources/sync` | `SyncGeaClientResourcesRequest` | `GeaClientResourceSyncResult` | AionUi Renderer | runtime token 被拒绝；写请求受 CSRF 保护 |

`resources` 接受 `assistants`、`skills`、`mcps`。当前实现只处理 `skills`：请求中不包含 `skills` 时返回 completed，并把所请求类型计入 skipped；这不代表 assistants 或 mcps 已完成同步。

## 4. AionCore → GEA

以下路径来自当前 AionCore 实现。GEA 官方接口规范仍是远端契约的最终权威；本地 wiremock 测试只能证明 AionCore 对这些形状的本地处理。

当前 AionUi 与 AionCore checkout 中没有检索到 GEA 官方 OpenAPI/Swagger 文件，因此下表不能替代 GEA 官方规范。取得官方规范后，应逐条建立对应关系并保留版本信息。

| 方法与 GEA 路径 | AionCore 用途 | 当前请求要点 | 当前响应校验 | 证据状态 |
| --- | --- | --- | --- | --- |
| `POST /ai/gateway/session` | 建立 Gateway Session | `consumerType`、`consumerCode`、`requestId`、`conversationId`、`channel`、可选 `preparationId` | `accessDecision.allowed`、`delegationToken`、gateway context 一致性 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/agent/session` | 统一 Session 路径返回 404 时的兼容回退 | `agentCode`、`channel` | 复用 Session 响应解析 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/mcp/proxy/list` | 查询会话授权工具 | Session、conversation、delegation token | 工具名唯一；校验 name/sourceCode；净化 inputSchema | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/mcp/proxy/call` | 调用授权工具 | Session、conversation、delegation token、mcpCode、toolName、arguments | sourceCode/toolName 必须与请求一致；提取 result/auditId | 代码实现；本地 mock 测试 |
| `GET /ai/gateway/interaction-requests` | 拉取会话 InteractionRequest | query 包含 agentCode、sessionId、conversationId；认证头包含用户登录态和 delegation token | 解析 revision 与 items，并更新本地投影 | 代码实现；本地 mock 测试 |
| `POST /ai/gateway/interaction-requests/{requestId}/actions` | 提交 InteractionRequest 动作 | Session 信息、expectedVersion、idempotencyKey、actionId、可选 payload | 解析回执并校验 requestId | 代码实现；本地 mock 测试 |
| `GET /aidata/client-resource-catalog/my` | 获取当前用户 Resource Catalog | 可选 revision query；使用用户 GEA 登录态 | 校验 status、revision、tenant 和 schema version | 代码实现；本地 mock 测试 |
| `GET /aidata/client-resource-catalog/skill-artifact` | 下载 Skill Markdown | skillCode、version、`format=md` | 校验响应头、长度、digest、version 和 UTF-8 | 代码实现；本地 mock 测试 |
| `POST /aidata/client-resource-catalog/skill-execute/report` | 上报受管 Skill 执行结果 | skill、version、digest、结果、耗时、大小及可选风险/错误信息 | 当前只检查 HTTP/业务错误 | 代码实现；本地 mock 测试 |

GEA Gateway 调用使用用户级 GEA 登录凭证；conversation 相关调用还带有当前会话的 delegation token。日志只记录低敏标识和状态，不应输出 token、请求体或工具结果。

## 5. Swagger 暴露方案

首批 Swagger 只覆盖本文接口，并遵守以下冻结方案：

- 文档入口只在 debug build 中注册。
- Swagger UI 和 OpenAPI JSON 仍经过现有 AionCore 认证中间件。
- Swagger UI 禁用全部 Try-it-out submit methods，避免从文档页面误触发读写调用。
- 禁用外部在线 validator，避免把接口规范发送给第三方服务。
- OpenAPI Schema 只能增加描述元数据，不能改变 serde 字段名、必填性、默认值或业务行为。
- 如 OpenAPI 工具无法无损描述当前接口，停止该项实现并记录提案，不调整接口迁就文档工具。

脱敏的本地与 GEA 出站请求样例见同目录的 `examples.http`。样例中的地址、身份和业务参数都是占位值；写请求只应在获得相应环境授权后手工执行。

## 6. 已发现但不在本项目修改的问题

| 现状 | 影响 | 处理方式 |
| --- | --- | --- |
| 全局列表兼容接受 `status=pending`，校验错误信息却只写“当前只支持 status=active” | 调试时可能误解兼容范围 | Swagger 按真实行为记录两个值；接口和错误文案均不修改 |
| AionUi 的 WebSocket adapter 只声明 `revision`，服务端事件还包含 `user_id` | TypeScript 调用方看不到完整载荷类型，但当前解析允许额外字段 | 文档记录真实载荷；不调整事件或前端类型 |
| 客户端资源枚举包含 assistants 和 mcps，但实现只处理 skills | 容易把“请求可接收”误认为“能力已支持” | Swagger 与文档明确标为 skipped 行为；不新增同步能力 |
| 当前证据主要来自代码和本地 mock/integration 测试 | 不能证明远端 GEA 当前部署与本文完全一致 | 保留真实 GEA 环境验收为独立待办，不提升验证状态 |
