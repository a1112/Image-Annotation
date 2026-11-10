# 移动协作开发版

服务地址：<https://175.178.16.90/image-annotation/>。当前工作区以 `8731402` 为基线，代码修改尚未提交或推送。

## 模块边界

| 目录 | 职责 |
| --- | --- |
| `src/App.tsx` | 应用装配、原桌面路由及移动壳选择 |
| `src/features/datasets`、`projects` | 数据集、项目与桌面工作台 |
| `src/features/annotation`、`src/annotation/pointer.ts` | 标注界面、画布几何、Pointer Events 手势 |
| `src/features/collaboration`、`mobile` | 账号、邀请、远端项目、任务、审阅、离线和冲突入口 |
| `src/features/shared`、`src/shared`、`src/platform` | 前台同步触发、图片地址、运行环境及共享视图 |
| `src-tauri/src/commands` | 账号、邀请与任务命令边界 |
| `project_fs.rs`、`credentials.rs` | 存储根、平台凭据与服务器/账号范围 |
| `storage.rs`、`hybrid.rs` | SQLite、持久 Outbox、base/local/remote 冲突与原子游标 |
| `sync_engine.rs`、`asset_cache.rs`、`publisher.rs` | 单项目同步租约、下载校验、缓存保护和桌面发布 |
| `server/src/routes/{account,collaboration,sync,objects}.rs` | 认证、任务/审阅、统一领域变更及私网对象代理 |

Rust 仍在原 crate 中逐步分层，旧命令名和项目目录继续兼容。移动端使用应用数据目录及受保护凭据存储；不运行本地 HTTP 后台、Python AI、托盘或桌面多窗口。桌面继续负责批量导入导出。

## 安装与操作

Android 开发包位于 `artifacts/mobile/image-annotation-arm64-debug-20261025.apk`，适用于 Android 7.0+ 的 arm64 设备。构建与签名/16KB 对齐证据见同目录 `build-receipt.json`。按本次验收约定，暂缓 Android 真机安装测试。

```sh
adb install -r artifacts/mobile/image-annotation-arm64-debug-20261025.apk
adb shell monkey -p com.imageannotation.desktop 1
```

iOS 原生工程位于 `src-tauri/gen/apple/image-annotation.xcodeproj`；最终 simulator App 位于 `artifacts/ios-simulator/app/Image Annotation.app`，设备端 unsigned IPA 位于 `artifacts/ios-device/Image Annotation-unsigned.ipa`。带 Keychain simulator entitlement 的可重建构建使用 `scripts/build-ios-simulator.sh`，签名与实际运行状态见 [iOS 验收记录](verification/2026-10-25-collaboration/ios-native-validation.md)。未获得有效开发签名的 IPA 不能直接安装到真机。

```sh
npm ci
npm run build
npm run tauri -- android build --target aarch64 --apk --debug
npm run tauri -- ios build --config src-tauri/tauri.ios.conf.json --open
# Simulator build: refresh Rust archives through Tauri, then supply simulator
# entitlements to xcodebuild; do not post-sign the app.
IMAGE_ANNOTATION_REBUILD_RUST=1 ./scripts/build-ios-simulator.sh
```

Android SDK/NDK、Java 与 Rust target 需按 [Tauri 移动环境要求](https://v2.tauri.app/start/prerequisites/#configure-for-mobile-targets) 配置。iOS 真机需按 [Tauri iOS 签名流程](https://v2.tauri.app/distribute/sign/ios/) 在 Xcode 中选择可用开发团队和描述文件。

桌面打开「协同登录」，使用账号密码登录后发布本地项目。项目所有者或管理员可生成一次性邀请码，人工分享给标注员和审阅员。移动端在「设置」中登录或接受邀请，打开远端项目后领取任务。任务中的原图可按需下载或保留离线；本地保存后触发同步，前台每 15 秒检查，网络恢复及返回前台立即检查。

离线保存、提交和审阅通过同一 SQLite 队列持久化。服务器确认后才显示正式完成；拒绝与冲突保留在状态中。保留本地冲突会基于最新远端版本生成新操作，重试原操作复用原操作 ID。服务器删除项目时，未同步草稿可显式保留为本地副本。缓存清理保护未同步草稿和固定资源。

旧桌面项目若 `remote_project_configs.account_id` 仍为空，不会继承当前登录会话的 token 进行同步；项目所有者需要重新发布，或执行显式远端配置以绑定当前服务器和账号。旧的项目凭据存取命令名仍兼容，但过期或未绑定的 project token 不再参与认证，需重新登录。

服务器和账号的远端缓存分别命名。首次管理员通过部署 CLI 创建；部署验收凭据保存在仓库之外、权限为 0600 的私有文件。密码和 token 不进入仓库或验收报告。

## 自动化验收

```sh
npm test
npm run build
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --manifest-path src-tauri/Cargo.toml --lib --test mobile_sync --locked
```

真实云端接口验收脚本会创建独立命名的测试项目和邀请账号；应先用于隔离环境，再用于获授权的上线验收：

```sh
python3 scripts/verify-collaboration.py --base-url https://HOST/image-annotation \
  --credentials /private/path/admin.json --report /tmp/collaboration-report.json
```

可单独运行 Rust 原生发布与双 SQLite 离线链路验收；不会在普通单元测试中自动请求服务器：

```sh
IMAGE_ANNOTATION_TEST_SERVER=https://HOST/image-annotation \
IMAGE_ANNOTATION_TEST_CREDENTIALS=/private/path/admin.json \
  cargo test --manifest-path src-tauri/Cargo.toml --test cloud_native -- --ignored
```

记录位于 [本轮验收目录](verification/2026-10-25-collaboration/)。接口测试、原生构建、设备安装、触控视觉和完整跨设备操作分别记录，不将其中任一项代替其余验收。

## 云端部署与回滚

线上继续使用 `serverall-image-annotation.service`、原 PostgreSQL、私网 MinIO 和 Nginx HTTPS 前缀。只新增迁移 `0003_mobile_collaboration.sql`，未修改已应用的 0001/0002。部署前在独立数据库、桶和回环端口演练，备份了数据库、对象、旧二进制、服务配置、环境文件及 Nginx 配置。

- 当前 release：`/home/ubuntu/projects/.deployment/releases/image-annotation/20261025-collaboration-0fbf08e250f1`
- 备份：`/home/ubuntu/projects/.deployment/image-annotation-collaboration-20261025/backup`
- systemd 覆盖：`/etc/systemd/system/serverall-image-annotation.service.d/90-codex-collaboration-20261025.conf`
- 部署源与制品校验值：[deployment-receipt.json](verification/2026-10-25-collaboration/deployment-receipt.json)

应用回滚时先停止写入，保存当前数据库备份，将旧二进制恢复为服务原路径，恢复备份的 `service.env`，移除上述本轮 systemd 覆盖后 `daemon-reload` 并重启。迁移只新增表/字段，优先保留新数据库；旧登录方式不具备本轮会话机制，回滚后需重新核验访问限制。

如需数据回滚，须在维护窗口停止服务，先保存当前数据库和对象，再用 `cutover-database.dump` 恢复到隔离数据库验证，切换连接后恢复对应对象快照。该步骤会舍弃切换后的协作数据，不能与常规应用回滚混用。Nginx 未修改，无需例行回退。恢复后检查 HTTPS `/health` 及项目读取。
