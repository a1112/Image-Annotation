# 2026-10-25 数据集托管验证汇总

本目录汇总数据集托管客户端、原生桥、服务端 API、隔离 PostgreSQL、故障恢复、生产部署和实际桌面/iOS 模拟器操作证据。Android 真机与 iOS 真机签名安装按用户选择延后，未计为通过。

## 测试与 API

| 范围 | 结果 | 证据 |
| --- | ---: | --- |
| 前端 Vitest | 197 项通过 | [frontend-tests.log](frontend-tests.log) |
| 前端生产构建 | 通过 | [frontend-build.log](frontend-build.log) |
| Chrome/native 前端壳测试 | 12 项通过 | [frontend-tests.log](frontend-tests.log) |
| Rust 原生客户端库 | 124 项通过 | [native-tests.log](native-tests.log) |
| Rust 移动同步集成 | 13 项通过 | [native-tests.log](native-tests.log) |
| 服务端单元测试 | 11 项通过 | [rust-tests.log](rust-tests.log) |
| 隔离 PostgreSQL 集成 | 5 项通过（2 collaboration + 3 dataset hosting） | [rust-tests.log](rust-tests.log) |
| 隔离 staging API | 71 项通过 | [staging-validation.json](staging-validation.json) |
| production HTTPS API | 69 项通过 | [production-validation.json](production-validation.json) |
| 故障注入/重启恢复 | 9 项通过，包含 SIGKILL 恢复、cleanup 失败配额保留 | [restart-validation.json](restart-validation.json) |
| 既有协作线上回归 | 34 项通过 | [collaboration-regression.json](collaboration-regression.json) |

staging 使用隔离服务和数据库；故障注入报告明确记录未触碰生产。生产 API 报告包含组织边界、ACL、幂等、版本不可变、native/COCO/YOLO/LabelMe 往返、ZIP 安全和配额检查。

## 部署

生产部署记录：

- source manifest：`caafd1e56a19290bf2a3ff075945a1868215135535489cd564c6c758c73c1e17`；
- binary SHA-256：`d6be8a4f6a9dc6e43fc03bbfb85844312f1ea7cd98d33fff14661d1efb1a94a6`；
- 服务：`serverall-image-annotation.service`；
- 新增迁移：`0004_dataset_hosting.sql`；既有 `0001`–`0003` 未修改；
- 部署前对象备份：28 个对象；
- 现有 HTTPS 前缀、私有 PostgreSQL/MinIO 和服务环境保留。

完整记录见 [deployment-receipt.json](deployment-receipt.json)。

### 回滚提醒

旧 binary 使用的 SQLx 可能拒绝包含 `0004` 的新数据库。回滚前必须在隔离恢复数据库上验证，或使用明确兼容 `0004` 的 rollback build；不要把旧 binary 直接留在已应用新迁移的生产数据库上。

## 移动制品

- Android：[hosting APK](../../../artifacts/mobile/image-annotation-hosting-arm64-debug-20261025.apk)，SHA-256 `474a4370ed905a51adfda5cca83722ad8ca5acbc8d0ea7c80ed732641ae81389`；验证包括 v2 签名、16KB zipalign/ELF、FileProvider 和 `hosting_open_export`。详见 [hosting-build-receipt.json](../../../artifacts/mobile/hosting-build-receipt.json)。
- iOS：[hosting unsigned IPA](../../../artifacts/ios-hosting-device/Image%20Annotation-hosting-unsigned.ipa)，SHA-256 `d52e7e78986233644c95ac046654c91e948783990d5ac80bb1d279a8c3e215e9`；arm64 未签名开发制品已通过 CRC 与内嵌资源校验；签名与真机安装按用户选择延后，未签名 IPA 不能直接安装到普通 iPhone。详见 [device receipt](../../../artifacts/ios-hosting-device/build-receipt.json)。
- 原协作 Android APK 保留，未被 hosting APK 覆盖。

## 实际界面与数据验收

桌面使用原生 Tauri 开发应用；手机和平板使用实际安装启动的 iOS Simulator 应用，登录现有 HTTPS 生产服务。

| 操作 | 已观察结果 | 证据 |
| --- | --- | --- |
| 桌面新建、收录与版本 | 独立数据集收录 3 张图片，创建 `v1-桌面发布`，冻结 9 个标注对象 | [成员界面](desktop-hosting-members.png)、[服务端快照](gui-server-state.json) |
| 桌面原生导出 | ZIP 下载完成，按钮在 Finder 定位正确文件 | [Finder](desktop-export-finder.png) |
| 桌面文件选择器导入 | 选取实际导出 ZIP，服务端 job 成功，恢复 3 条数据、9 个对象和 1 个版本 | [导入完成](desktop-native-import-completed.png)、[最终服务器核验](final-server-audit.json) |
| iPhone 查看与跨项目复用 | 查看共享数据集并复用至 `MobileHostingReuse2026-10-26`；3 图、3 类别和独立标注落地 | [数据集](iphone-hosting-dataset.png)、[复用项目](iphone-reused-project.png)、[本地文件核验](gui-local-files.json) |
| iPhone 打开原图与标注 | 修复移动路由后，最终应用打开 demo_001 原图和 5 个框；全部三张原图 SHA/大小匹配 | [标注工作台](iphone-reused-annotations.png)、[本地文件核验](gui-local-files.json) |
| iPhone 原生导出 | ZIP 保存至 Documents/Dataset Exports，3 条数据/9 个对象，CRC 与 SHA 通过；与桌面 ZIP 字节相同 | [本地文件核验](gui-local-files.json) |
| iPad 横屏 | 同一数据集、成员和版本 manifest 可见，查看者写入口禁用 | [横屏](ipad-hosting-landscape.png)、[最终版本详情](ipad-version-manifest-final.png) |

GUI 过程中发现并修复了移动端“继续标注”调用桌面窗口、导入格式控件宽度及导出长路径/版本清单撑宽问题；最后修复已进入 197 项测试、APK、IPA 和 Simulator 应用。最终 Simulator 已在 iPhone 17 Pro Max 与 iPad Air 11-inch (M4)、iOS 26.5 安装启动。解锁后已完成最终复查：手机拖动标注并保存，服务器修订从 1 升到 2，原数据集版本 hash 保持不变；手机导出文件名正常换行并显示 Files 位置；iPad manifest 正常换行，面板完整可见。见 [保存后工作台](iphone-reused-save-synced.png)、[最终手机导出](iphone-export-final.png)、[最终平板版本详情](ipad-version-manifest-final.png) 和 [服务器保存闭环](final-server-audit.json)。修复前截图 `iphone-export-before-wrap-fix.png` 仅作为问题修复前的历史证据保留。临时唤醒已结束。

## 最终制品与运行边界

- [最终制品 CRC/hash 核验](final-artifact-validation.json)；[Simulator receipt](../../../artifacts/ios-hosting-simulator/build-receipt.json)。
- 最终前端资源：`index-BXBDhCpR.js`、`index-DI0zy5Wd.css`；[source manifest](ios-hosting-final-source-manifest.json)、[dist manifest](ios-hosting-final-dist-manifest.json)。
- 桌面 [Hosting Development.app](../../../artifacts/desktop/Image%20Annotation%20Hosting%20Development.app) 依赖本机 Vite `http://localhost:1440`；它是已运行验收的开发应用，未作为独立 macOS 发布包交付。[desktop receipt](../../../artifacts/desktop/development-build-receipt.json)。
- 生产服务运行正常；隔离 staging 服务与故障代理已停止，数据库、存储桶和备份保留。回滚需遵循上述迁移兼容性限制。
- 用户明确延后 Android 真机及 iOS 真机签名安装，本轮没有以构建结果代替这些验收。
