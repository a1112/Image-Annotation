# 2026-10-25 移动协作交付与验收

代码基线为 `8731402`，本轮修改留在工作区，未提交或推送。远端同步已复核 `HEAD...origin/main = 0 0`。架构、安装和回滚说明见 [移动协作开发版](../../mobile-collaboration.md)。

## 构建与验证

| 范围 | 结果 | 证据 |
| --- | --- | --- |
| 前端 | 161 项 Vitest、12 项 Chrome 测试与生产构建通过 | [frontend-tests.log](frontend-tests.log)、[frontend-build.log](frontend-build.log) |
| Rust 桌面及共享核心 | 111 项库测试、13 项同步测试通过 | [rust-tests.log](rust-tests.log) |
| 原生客户端与真实云端 | 1 项完整链路通过：桌面发布、未打开 YOLO 标签、终态失败恢复、重复发布、双 SQLite 离线冲突、连续保存提交、审阅与删除 | [native-cloud-tests.log](native-cloud-tests.log) |
| PostgreSQL 集成 | 2 项测试在隔离数据库实际执行通过 | [server-pg-tests.log](server-pg-tests.log) |
| 隔离服务 API | 34 项通过，包含跨组织访问拒绝 | [staging-validation.json](staging-validation.json) |
| 线上 HTTPS API | 33 项通过，包含登录/刷新/撤销、权限、任务占用、幂等、版本冲突、对象及缩略图校验 | [production-validation.json](production-validation.json) |
| 并发提交与游标 | 人为延迟第一事务提交，验证第二事务不能越过项目锁；中途 bootstrap 和增量拉取保持一致，15 项通过 | [commit-order-validation.json](commit-order-validation.json) |
| 接口文档 | 严格 YAML 检查无重复键，37/37 路由路径与运行时代码一致 | `server/openapi.yaml` |
| 桌面原生 GUI | 标注、撤销重做、保存、LabelMe 导入导出、切图与重启恢复、发布 3 图及 3 份标注、选单图创建任务；最终回读 5 框及通过状态 | [desktop-ui-validation.json](desktop-ui-validation.json)、[最终截图](desktop-approved-five-boxes.png) |
| iPhone Simulator | 原生密码登录、Keychain 重启恢复、领取任务、固定单图离线、画框/撤销重做、保存提交后自动同步；最终 5 框、抽屉关闭及横竖屏检查通过，3 份原图哈希一致 | [mobile-ui-validation.json](mobile-ui-validation.json)、[iphone-cache-validation.json](iphone-cache-validation.json)、[最终截图](iphone-approved-final.png) |
| iPad Simulator | 审阅员登录、查看手机提交的 5 框、通过版本 5 的标注；最终版本 6 已通过，横屏分栏显示正常 | [审阅截图](ipad-approved.png)、[横屏截图](ipad-landscape-approved.png)、[服务端复核](cross-device-server-validation.json) |

桌面 GUI 运行的是 `artifacts/desktop/Image Annotation Development.app` 与本地 Vite 开发服务器，非新增的桌面发布安装包。服务器、原生测试与 GUI 各自保留证据。

## 移动制品与设备范围

- Android：[image-annotation-arm64-debug-20261025.apk](../../../artifacts/mobile/image-annotation-arm64-debug-20261025.apk)。最终 APK SHA-256 为 `46cb78cf52b61cf5a559c0feeeed962f0098800ce05da0106a4a935615ae8eab`。构建、签名、16 KB ZIP/ELF 对齐和源码校验记录见 [android-validation.md](android-validation.md) 及制品同目录 `build-receipt.json`。依用户本轮选择，验收 APK 构建，真机安装待设备可用。
- iOS：[Image Annotation-unsigned.ipa](../../../artifacts/ios-device/Image%20Annotation-unsigned.ipa)，SHA-256 为 `74e27c26bf144db81c89c6cac9eab0fc5a7f1b9a129b96873bea6f1cd43b5cbb`。依用户本轮选择交付未签名 IPA；开发签名和真机安装稍后处理。构建记录在同目录 `build-receipt.json`。
- iOS Simulator：`artifacts/ios-simulator/app/Image Annotation.app`。Xcode 在链接签名时注入 simulator Keychain entitlement，未使用明文凭据降级。设备及视觉记录见 [ios-native-validation.md](ios-native-validation.md)。

模拟器视觉操作、真实硬件触控、多点手势、签名制品安装是不同验收项。双指几何由自动化测试覆盖，不能据此宣称完成真实硬件双指触控验收。

最终 Android、iOS device 和 simulator receipt 的共享源码清单均为 `3d22fa4f4783f5ecc48c3f38bf462a34df1db012aa766db63fdeb0ef4ee4de6b`，前端资源清单均为 `03ab4d785c4d23738584bbb31b2ad84ee3369c98149feff5edc06d30a51e3e72`。逐文件清单见 [client-source-manifest.json](client-source-manifest.json) 与 [frontend-dist-manifest.json](frontend-dist-manifest.json)，各自记录可复现算法。旧空白预览制品已替换；移动命令现已注册 `get_file_asset_path`。

## 跨设备操作与剩余验收范围

已完成实际 GUI 闭环：桌面发布项目并创建单图任务 → iPhone Simulator 领取、画框、保存及提交，自动同步 5 框到服务器版本 5 → iPad Simulator 查看并通过该版本，服务器标注进入版本 6 → 桌面切图刷新后显示 5 框及「通过」。保存/提交未依赖设置页手动上传。服务端和本地 SQLite 分别复核队列及版本，详见 [移动交互报告](mobile-ui-validation.json) 和 [服务端状态](cross-device-server-validation.json)。最终制品在抽屉独立关闭按钮补丁后重新安装，已复核登录恢复、图片、通过状态、抽屉、横竖屏、平板分栏及离线固定。

Android 真机安装、iOS 真机签名及安装按用户决定延期。实体设备双指触控、双设备同时断网的完整 GUI 场景尚未验收；真实云端与双 SQLite 客户端的离线冲突、连续保存提交及重启恢复自动化已通过，二者分别记录。

## 部署记录

线上继续使用 `serverall-image-annotation.service`，回环端口 `16423`，现有 Nginx HTTPS 前缀及私网 MinIO。只新增数据库迁移 `0003_mobile_collaboration.sql`，保留 0001/0002 内容和校验值。上线前备份数据库、对象、二进制、环境和服务配置。制品及备份位置见 [deployment-receipt.json](deployment-receipt.json)。

并发验证完成后，已停止本轮临时隔离服务 `serverall-image-annotation-collaboration-final-staging.service`；隔离数据库、桶、日志和备份保留。生产服务保持 active，`/health` 返回正常。

账号密码、邀请密钥及会话 token 不进入仓库、截图或报告。测试账号与管理员凭据仅存本机权限为 0600 的私有目录，运行日志不打印凭据。
