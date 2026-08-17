# 托管实时语音

本模块为 AionUi 提供火山引擎 RTC 智能体的最小服务端边界。浏览器只获得短期 RTC Token；RTC AppKey、火山 AK/SK 和完整智能体配置只保留在 AionCore 进程内存中，不写入数据库，也不会通过 API 返回。

## 配置

启动 AionCore 前设置以下环境变量：

| 环境变量 | 内容 |
| --- | --- |
| `VOLC_ACCESSKEY` | 火山引擎账号 AK |
| `VOLC_SECRETKEY` | 火山引擎账号 SK |
| `AIONUI_VOLCENGINE_RTC_APP_ID` | RTC AppId，固定 24 字符 |
| `AIONUI_VOLCENGINE_RTC_APP_KEY` | RTC AppKey |
| `AIONUI_VOLCENGINE_VOICE_CHAT_CONFIG` | `StartVoiceChat` 请求中的完整 `VoiceChat` JSON 对象 |

`AIONUI_VOLCENGINE_VOICE_CHAT_CONFIG` 可从火山 RTC AIGC 控制台“接入 API”生成的配置中提取。对象至少应包含非空的 `AgentConfig.UserId` 和完整的 `Config`。AionCore 会在每次会话中覆盖 `AppId`、`RoomId`、`TaskId` 和 `AgentConfig.TargetUserId`，不要依赖模板中的这些字段。

任一配置缺失或 JSON 无效时，`GET /api/voice/capabilities` 返回 `enabled: false`，创建会话返回 `VOICE_NOT_CONFIGURED`，不会尝试调用云端。

## 会话时序

1. `POST /api/voice/sessions` 生成短期 RTC Token，但不启动智能体。
2. AionUi 使用返回的 `app_id`、`room_id`、`user_id`、`token` 加入 RTC 房间。
3. 进房成功后调用 `POST /api/voice/sessions/{session_id}/start`，AionCore 签名调用火山 `StartVoiceChat`。
4. `DELETE /api/voice/sessions/{session_id}` 调用 `StopVoiceChat`；重复结束同一会话保持成功。

所有接口均受 AionCore 鉴权保护，状态变更接口还受 CSRF 和认证用户级限流保护。会话绑定创建用户，跨用户访问统一返回 `VOICE_SESSION_NOT_FOUND`。

阶段 A 不实现业务工具调用或写操作确认；真实可用性仍需使用已开通 RTC、ASR、TTS 和模型资源的火山账号完成浏览器联调。
