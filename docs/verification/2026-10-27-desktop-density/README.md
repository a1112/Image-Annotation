# 紧凑桌面工作区

整体管理、项目数据和标注工作台采用统一的中性灰、细边框和小圆角。重复标题和说明已删减；项目工具、图像统计、筛选与分页优先同排。数据集卡片以名称、数量、状态和进度区分层级，整理操作收进省略菜单；标注的保存和切图保持可见，高级操作放入菜单。

- 前端 272 项与窗口相关 12 项测试通过：[日志](../../../artifacts/desktop-density-20261027/frontend-tests.log)。
- 网页实操通过：菜单移动与撤销、分组展开、图像管理、单行标注工具栏。1280 px 视口下，项目标题栏 40 px，数据工具栏 39 px。
- macOS 桌面重新打包并启动隔离验收副本；实测卡片拖拽、撤销、标注工具栏及高级菜单。修复网格自动拉伸标题栏的问题。
- app 和运行副本的 30 个 Mach-O 文件区段一致，签名严格校验、DMG 校验及挂载内容比对通过；使用 ad-hoc 签名。[包回执](../../../artifacts/desktop-density-20261027/package-receipt.json)
- 生产仅更新静态资源：98 override，服务端二进制、数据库和对象存储保持不变；旧 hashed assets 保留。分组、历史和图像标签引用的 3 项只读回归通过。[部署回执](../../../artifacts/desktop-density-20261027/production-static-apply-final.json)

[网页版](https://175.178.16.90/image-annotation/app/) · [最终验证](../../../artifacts/desktop-density-20261027/verification.json) · [源码变更清单](../../../artifacts/desktop-density-20261027/source-changes.json)

![项目数据](../../../artifacts/desktop-density-20261027/web-project-data.png)

![桌面标注](../../../artifacts/desktop-density-20261027/desktop-annotation.png)
