# PDF 译稿前端

React + Vite + TypeScript 前端，用于创建 PDF 翻译任务、跟踪实时进度、取消任务、下载结果，以及在 `/admin` 管理服务器端翻译渠道。

## 本地开发

```bash
npm install
npm run dev
```

默认将 `/api` 代理到 `http://127.0.0.1:7860`。可在启动前设置 `VITE_DEV_API_TARGET` 修改开发代理目标。如果 API 部署在不同域名，构建时可设置 `VITE_API_BASE_URL`。

## 验证与构建

```bash
npm test
npm run build
```

生产文件输出到 `dist/`。后端需要在同一来源下提供 `/api/*` 接口，并将未知非 API 路径回退到 `index.html`，以便直接访问 `/admin`。

## 任务生命周期

- 页面将当前任务 ID 保存在当前标签页会话、将表单设置保存在浏览器中；刷新后通过 `GET /api/jobs/:id` 恢复，同时避免其他标签关闭时误取消本页任务。
- 运行中通过 SSE 接收进度，同时使用低频轮询作为断线保底。
- 活跃任务每 10 秒发送 heartbeat。
- 页面离开时通过 `sendBeacon` 发送 release；刷新或从浏览器往返缓存返回后会立即续 heartbeat，真正关闭且超过服务端宽限期后才取消任务。
- 失败或取消后可使用保留在当前页面的 PDF 重新提交；刷新后需重新选择文件。

## API 约定

前端使用如下端点：

- `GET /api/health`
- `GET /api/config`
- `POST /api/jobs`
- `GET /api/jobs/:id`
- `GET /api/jobs/:id/events`
- `POST /api/jobs/:id/cancel`
- `POST /api/jobs/:id/heartbeat`
- `POST /api/jobs/:id/release`
- `GET /api/jobs/:id/files/mono|dual|glossary`

管理页面使用同源 HttpOnly 会话 Cookie。API 密钥只会在保存渠道时作为请求体提交，不写入浏览器存储，也不会由服务器回显。管理端点包括：

- `GET /api/admin/session`
- `POST /api/admin/setup`
- `POST /api/admin/login`
- `POST /api/admin/logout`
- `GET|POST /api/admin/providers`
- `PUT /api/admin/providers/deepseek`
- `PUT|DELETE /api/admin/providers/:id`
