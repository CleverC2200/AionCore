# AionCore GEA 接口

本 crate 负责 AionCore 的 GEA 登录态、Gateway Session、MCP 工具、InteractionRequest 和 Resource Catalog 集成。

## 查看接口

debug build 启动后，在当前 AionCore 地址访问：

- OpenAPI JSON：`/openapi.json`

OpenAPI JSON 经过现有 AionCore 认证；release build 不注册该入口。Swagger UI 页面和专用依赖已移除，OpenAPI 契约生成保留。

## 当前接口资料

- [接口现状索引](docs/api.zh-CN.md)
- [脱敏 HTTP 联调样例](docs/examples.http)

这些资料只描述当前实现。任何路由、字段、响应、鉴权、错误语义或调用行为变更都需要先单独审批。
