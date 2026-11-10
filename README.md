# Image Annotation

图像标注项目，包含 React 前端、Tauri 原生客户端和 Rust 协作服务。桌面端与 Web 端共用前端代码；服务端的配置和部署说明见 [server/README.md](server/README.md)。

## 项目目录

- `src/`：前端。`features/` 按标注、数据集、项目、协作、Web 等功能组织界面；`api/` 封装服务调用；`platform/` 区分运行环境；`shared/` 存放共享导航和状态辅助代码；`types/` 存放领域类型。`annotation/` 包含标注编辑器的基础状态与指针逻辑。
- `src-tauri/`：Tauri 客户端与 Rust 原生能力，包括命令、导入器、本地项目、缓存和同步；`tests/` 和 `examples/` 分别存放集成测试与示例。
- `server/`：Rust 协作服务，包含路由、数据库迁移、OpenAPI 定义和部署配置。
- `scripts/`：开发启动、构建、部署和验证脚本；`scripts/ai/` 包含 AI 桥接脚本及依赖说明。
- `docs/`：设计、协议、实施计划和验证记录。
- `data/`、`artifacts/`：本地数据与验证产物，已被 Git 忽略；`dist/`、`dist-web/`、各 Rust `target/` 和 `node_modules/` 为构建产物或依赖。

## 常用命令

安装依赖后，在仓库根目录运行：

前端工具链（包括 Vitest 4）支持 Node.js 20、22 或 24 及更高版本，建议使用当前维护中的 Node.js LTS；npm 需与 Node.js 一起安装。

```sh
npm install
npm run dev         # 启动开发环境
npm run build       # 构建桌面前端
npm run build:web   # 构建 Web 前端
npm test            # 前端和窗口控件测试
```

原生客户端和服务端的构建需要各自的 Rust/Tauri 环境；服务端的数据库及环境变量配置见其 README。部署和验证脚本可能需要特定环境，使用前请先阅读脚本及对应文档。
