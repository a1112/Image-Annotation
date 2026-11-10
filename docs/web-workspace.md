# Web 工作区

Web 端是服务端数据工作区的浏览器入口，桌面 Tauri 应用仍独立使用本地原生桥和本地构建产物。Web 不依赖 `localhost:17310`，也不读取本机路径或桌面 AI 能力。

## 架构边界

- 浏览器入口是 `WebWorkspaceApp`。`src/main.tsx` 在普通浏览器中选择它；Tauri 运行时，或显式设置 `VITE_APP_MODE=desktop`，仍选择桌面 `App`。
- 登录使用服务端 HttpOnly Cookie：`POST /api/v1/web/auth/login`、`GET /api/v1/web/auth/session`、`POST /api/v1/web/auth/refresh` 和 `POST /api/v1/web/auth/logout`。请求携带 same-origin Cookie，不把访问令牌放入 Web Storage。
- 数据和权限以服务端 PostgreSQL、S3 对象存储为准。项目、数据集、分支、标注、提交、任务和转换历史不以浏览器缓存作为权威；图像上传和内容读取也通过认证服务端接口完成。
- DTO 命令使用 `POST /api/v1/web/commands/{command}`，响应为 `{ok,data}`。浏览器数据集 ID 使用 `projectUUID::canonicalKey`，不要把它当作本地文件路径或桌面缓存 ID。
- Web 图像转换提交到 `/api/v1/data-projects/{projectId}/image-transforms`，任务状态和持久历史从服务端读取。输出是新对象，原图与标签不会被面板覆盖；下载继续经过认证下载路由。

## 部署路径与构建

生产静态页面预期挂载在：

```text
/image-annotation/app/
```

在没有 `VITE_REMOTE_API_URL` 时，Web API 基址会从当前 URL 去掉末尾 `/app` 得到 `/image-annotation`，因此不会默认请求回环地址。跨域或反向代理部署需要在构建时显式设置 `VITE_REMOTE_API_URL`，并确保 Cookie、Origin/CSRF 和 API 代理策略一致。

```bash
npm run build:web   # 输出 dist-web，静态基址为 ./
npm run build       # 桌面构建，继续输出 dist
```

`dist-web/` 是构建产物，已加入 `.gitignore`，不应作为源码提交。

## 浏览器使用路径

1. 打开 `/image-annotation/app/`，登录服务端账户。
2. 在数据项目首页查看项目与数据集；创建数据集、上传图像和归属操作直接保存到服务端。
3. 打开数据集进入浏览器标注工作台，标注保存走 Web 命令接口。
4. 在图像转换面板选择服务端返回的图像记录，提交格式、尺寸、裁剪、旋转或翻转参数。只有服务端提供 `imageHash` 时才会显示可提交的转换控制。
5. 转换任务会轮询状态；刷新页面后，面板从持久历史接口恢复任务状态和下载入口。找不到源文件名时会显示“服务端图像”，不会用浏览器本地哈希猜测源对象。

Web 模式隐藏桌面窗口控制、本地路径、本地 AI、测试数据下载和云发布/同步重复入口；这些能力仍由桌面模式独立负责。

## 验收状态

当前源码验证已通过（2026-10-27）：

- 完整 `npm test`：21 个 Vitest 文件、237 个测试，以及 12 个 Chrome/native 测试通过；
- `npm run build:web`：Web TypeScript 检查和 Vite 构建通过；
- Web focused 测试覆盖登录/会话、数据集加载与上传、服务端转换提交、持久转换历史、下载、缺少 `sourceHash`、数据集切换旧请求防回写。

详细计数与运行时门槛见[本轮 Web 工作区验证记录](verification/2026-10-26-web-workspace/README.md)。

### 已完成的正式验收

正式环境已部署并通过实际验收：PG/S3、HttpOnly Cookie 登录、真实图像上传与标注、分支合并及回退、服务重启恢复、常用格式转换输出和浏览器下载均已通过。桌面来源的数据集可在 Web 直接打开并继续上传和标注；真实原生桌面也已克隆读取 Web 提交。

正式 Web 操作 34 项、图像转换 43 项、原生云协议 30 项及 Edge 界面 9 项通过；隔离服务重启后的项目恢复 8 项、转换恢复 3 项通过。源码验证为 237 个 Vitest、12 个 Chrome 壳、29 个服务端单元测试和 11 个真实 PostgreSQL 集成测试。

正式入口：[Web 数据工作区](https://175.178.16.90/image-annotation/app/)。部署回执和截图见 [最终验收记录](verification/2026-10-26-web-workspace/README.md)。当前 Web 导出为 JSON 项目清单和原图/标签逐项下载，整包导入导出与本地 AI 继续由桌面处理。
