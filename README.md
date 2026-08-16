# Argus

Argus 是一个 Rust 编写的插件化可用性监控器。当前内置 GetLLM 文字、文生图探针和飞书应用机器人告警；每个探针独立调度，后续可以继续增加其他监控插件。

## 行为

- 文字探针请求 OpenAI 兼容的 `POST /v1/chat/completions`，默认提示词为 `ping`，输出上限为 1 token。
- 文生图探针先请求 `GET /v1/models`，确认 `gpt-image-2` 存在且具备图片输出能力，再请求 `POST /v1/images/generations`。默认只生成一张低质量图片，流式校验并丢弃响应内容，不保存图片。
- 首次启动立即检查，此后按配置间隔执行；错过的周期会跳过，不会并发堆积。
- 达到连续失败阈值后只发送一次告警；恢复达到阈值后发送一次恢复通知。飞书发送失败会在下一轮重试。
- API Key、飞书 App Secret 和访问 Token 只从环境变量或飞书鉴权接口读取，不会写入日志。

## 配置

```bash
cp .env.example .env
```

填写 `.env` 中的 `GETLLM_API_KEY`、`FEISHU_APP_ID`、`FEISHU_APP_SECRET` 和目标群的 `FEISHU_CHAT_ID`。飞书应用需要开启机器人能力、申请 `im:message:send_as_bot` 权限、发布版本，并加入目标群。

程序会使用 App ID/App Secret 获取并缓存 `tenant_access_token`，然后通过飞书发送消息 OpenAPI 向目标群主动告警。此场景不接收飞书事件，因此无需 Webhook URL、公网回调地址或长连接。

默认文字探针每 1 分钟执行一次，文生图探针每 10 分钟执行一次：

```dotenv
GETLLM_TEXT_INTERVAL_SECS=60
GETLLM_IMAGE_INTERVAL_SECS=600
```

如需所有探针使用相同频率，可设置兼容配置 `ARGUS_INTERVAL_SECS`；单独设置的探针频率优先级更高。

`GET /v1/models` 不产生模型 Token；只有模型存在且能力匹配时才会发起付费的生图请求。可通过 `GETLLM_TEXT_ENABLED` 或 `GETLLM_IMAGE_ENABLED` 单独停用探针。

## 本地运行

```bash
# 只执行一次探针，不发送飞书告警
cargo run -p argus-server -- --once

# 持续监控
cargo run -p argus-server
```

## Docker Compose 发布

先补齐 `.env` 中的飞书应用凭证和目标群 ID，然后运行：

```bash
./scripts/release.sh
```

常用运维命令：

```bash
docker compose logs -f argus
docker compose ps
docker compose down
```

容器以非 root 用户运行，并启用只读根文件系统、移除 Linux capabilities 和 `no-new-privileges`。

## 插件结构

```text
src/core              领域类型，无异步依赖
src/runtime           MonitorPlugin / Notifier 接口、调度与事件状态机
src/plugin-getllm     GetLLM 文字及文生图插件
src/notifier-feishu   飞书应用机器人 OpenAPI 通知插件
src/server            配置与组装入口
```

新增任务时实现 `argus_runtime::MonitorPlugin`，然后在 `src/server` 注册即可；底层 crate 不依赖具体插件，依赖关系保持无环。
