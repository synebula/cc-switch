# CC Switch Web 模式（非 Tauri）

本目录提供一个“尽量不改上游 `src/`”的 Web-only 入口，通过 Vite alias 将 `@tauri-apps/*` 导入指向本地 shim，并把原本的 `invoke()` 调用转发到运行中的 Web 后端（headless）。

## 运行

## 连接服务端（推荐：管理服务端配置）

如果你希望 **Web UI 管理“服务端正在运行的配置”**，可以启动本仓库提供的最小后端：

```bash
cargo run -p cc-switch-service
```

然后在启动 Vite 时指定后端地址：

```bash
VITE_CC_SWITCH_BACKEND_URL=http://127.0.0.1:8787 ./node_modules/.bin/vite -c vite.web.config.ts
```

构建（产物在 `dist-web/`）：

```bash
VITE_CC_SWITCH_BACKEND_URL=http://127.0.0.1:8787 ./node_modules/.bin/vite build -c vite.web.config.ts
```

说明：

- 后端持久化目录复用 `src-tauri` 的路径逻辑（默认写入 `~/.cc-switch/`）。
  - 若需显式覆盖 home（例如 Docker 挂载），可设置 `CC_SWITCH_TEST_HOME=/path/to/home`。
- 若导入 SQL 备份时报 `413 Payload Too Large`，可调整后端请求体上限：`CC_SWITCH_WEB_MAX_BODY_BYTES`（默认 `200 * 1024 * 1024`）。
- 若要开启 Token 鉴权：后端 `CC_SWITCH_WEB_AUTH_TOKEN=...`，前端 `VITE_CC_SWITCH_BACKEND_TOKEN=...`。
- 事件订阅当前用 SSE；`EventSource` 不支持自定义 Authorization Header，本项目通过 `?token=` 兼容（生产建议改成同源 Cookie 或 WebSocket）。

## Docker（完整服务：Web UI + API）

该容器同时提供：

- Web UI：`GET /`
- API：`POST /invoke`
- 事件：`GET /events`（SSE）
- 代理服务：默认自启（Docker 镜像默认端口 `5000`，可在 UI 中配置）

构建镜像（Dockerfile 内会构建 `dist-web/` + Rust 二进制）：

```bash
docker build -t cc-switch-service:local .
```

运行（持久化目录挂载到宿主机 home 下）：

```bash
docker run --rm -p 8787:8787 \
  -p 5000:5000 \
  -v "$HOME:/host-home" \
  -e CC_SWITCH_TEST_HOME=/host-home \
  -e CC_SWITCH_WEB_AUTH_TOKEN=change-me \
  cc-switch-service:local
```

注意：挂载整个 `$HOME` 赋予容器读写你 home 的权限，建议使用专用子目录或最小权限挂载。

## 一键脚本

仓库根目录提供脚本：

  - 已移除（这些脚本不参与 GitHub Actions 发布流程）。建议直接使用上文的 `cargo` / `docker` 命令。

## 说明

- 代理服务、系统托盘、写入本地 CLI 配置目录、终端拉起、原生更新等依赖桌面能力的功能在 Web 模式下不可用。
