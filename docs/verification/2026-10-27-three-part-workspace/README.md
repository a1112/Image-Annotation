# 三部分工作区与数据集分组最终验收

本页记录 2026-10-27 的隔离 fixture、staging、最终桌面包和生产复核证据。报告不包含账号、密码或 token；生产 Web 入口为 [https://175.178.16.90/image-annotation/app/](https://175.178.16.90/image-annotation/app/)。

## 已通过的证据

### Web/desktop 三部分工作区

当前页面和路由测试覆盖三部分工作区边界：

1. 整体项目管理：项目摘要、项目入口和项目切换。
2. 项目内数据管理：数据集、分组、版本和协作入口。
3. 数据标注工作区：图像画布、类别、标签和保存流程。

前端自动化测试通过 269 项，Chrome 原生窗口相关测试通过 12 项：[frontend-tests.log](../../../artifacts/three-part-workspace-20261027/frontend-tests.log)。项目/工作区路由测试记录在 [navigation-tests-final.log](../../../artifacts/three-part-workspace-20261027/navigation-tests-final.log)。

最终 GUI 回执为 12 项 Web 检查和 9 项 desktop 检查，覆盖拖拽分组、数据管理、真实画布框选与标签保存、viewer 只读、未保存离开提示以及桌面重启后的数据保留：[gui-verification.json](../../../artifacts/three-part-workspace-20261027/gui-verification.json)。截图证据：

- Web：[拖拽分组](../../../artifacts/three-part-workspace-20261027/web-drag-highlight.png)、[数据集管理](../../../artifacts/three-part-workspace-20261027/web-dataset-management.png)、[标注保存](../../../artifacts/three-part-workspace-20261027/web-annotation-save.png)、[viewer 分组](../../../artifacts/three-part-workspace-20261027/web-viewer-groups.png)、[viewer 版本](../../../artifacts/three-part-workspace-20261027/web-viewer-versions.png)、[未保存提示](../../../artifacts/three-part-workspace-20261027/web-unsaved-dialog.png)。
- Desktop：[项目主页](../../../artifacts/three-part-workspace-20261027/desktop-project-home.png)、[项目数据页](../../../artifacts/three-part-workspace-20261027/desktop-project-data-final.png)、[标注保存](../../../artifacts/three-part-workspace-20261027/desktop-annotation-save.png)、[未保存提示](../../../artifacts/three-part-workspace-20261027/desktop-unsaved-dialog.png)。

### 分组、版本和权限

- native 分组 fixture 22/22 通过，覆盖稳定分组 ID、单层成员归属、CAS、仅分组变更提交、分支隔离、合并、回退和历史保留：[native-groups.json](../../../artifacts/three-part-workspace-20261027/native-groups.json)。
- staging Web 分组验收 25/25 通过，覆盖项目作用域数据集、分组 CAS、分支/合并/冲突/回退和图像标签引用保留：[staging-groups.json](../../../artifacts/three-part-workspace-20261027/staging-groups.json)。
- staging ACL 验收 5/5 通过：非成员拒绝读取、owner 授予 viewer、viewer 可读但 `canEdit=false`、viewer 写入拒绝且 revision 不变：[staging-permissions.json](../../../artifacts/three-part-workspace-20261027/staging-permissions.json)。
- native `--lib` 最终测试 150/150 通过：[native-lib-final-after-metadata-tests.log](../../../artifacts/three-part-workspace-20261027/native-lib-final-after-metadata-tests.log)。新增 metadata merge 单测 4/4 通过：[native-metadata-merge-tests.log](../../../artifacts/three-part-workspace-20261027/native-metadata-merge-tests.log)。
- 最终打包 fixture 的分组检查 22/22 通过，native 重启检查 3/3 通过：[packaged-final-native-groups.json](../../../artifacts/three-part-workspace-20261027/packaged-final-native-groups.json)、[packaged-final-restart.json](../../../artifacts/three-part-workspace-20261027/packaged-final-restart.json)。
- clean/dirty 工作树刷新检查 2/2 通过：clean 工作树跟随 native ref，dirty 工作树不被后续 native ref 覆盖：[native-web-clean-refresh.json](../../../artifacts/three-part-workspace-20261027/native-web-clean-refresh.json)。

三方 metadata 合并规则已由测试覆盖：独立分组修改自动合并；同一分组删除与修改产生 whole-group conflict；同一数据集移向不同分组产生归属冲突；目标分支顺序优先，incoming-only 成员追加。

### Native ↔ Web 云互通

隔离 staging 三端验收 20/20 通过：[native-web-group-sync.json](../../../artifacts/three-part-workspace-20261027/native-web-group-sync.json)。流程覆盖 native A 发布、native B 克隆、Web 读取并改名、A/B 拉取、A 新增第三分组并推送、Web clean-worktree 刷新、B 再拉取；图像 bytes、标签实际值、第二父提交及 immutable object identity 均保持一致。

### 旧协议兼容

协议兼容验收 8/8 通过：[protocol-compatibility.json](../../../artifacts/three-part-workspace-20261027/protocol-compatibility.json)。

- 已启用分组的项目收到不含 `projectMetadata` 的旧 REST push 时，明确返回 `PROJECT_METADATA_REQUIRED`。
- 被拒绝的旧 push 不改变 remote revision 或 refs。
- 未启用分组的 legacy commit 可以读取，缺少 `projectMetadata` 时按空 `datasetGroups` 处理。
- 外来 dataset key 与分支作用域拒绝已由 [staging-groups.json](../../../artifacts/three-part-workspace-20261027/staging-groups.json) 中的项目作用域和 foreign-key 检查覆盖。

## 生产验收

生产 API 验收合计 115 项，分项为 25（分组）+ 34（Web workspace）+ 5（权限）+ 8（协议兼容）+ 43（图像转换）：

- [production-groups.json](../../../artifacts/three-part-workspace-20261027/production-groups.json)：25/25。
- [production-web-workspace.json](../../../artifacts/three-part-workspace-20261027/production-web-workspace.json)：34/34，包含真实图像/标签、CAS、三方合并、冲突解决、restore、CSRF 和过期 cookie。
- [production-permissions.json](../../../artifacts/three-part-workspace-20261027/production-permissions.json)：5/5。
- [production-protocol-compatibility.json](../../../artifacts/three-part-workspace-20261027/production-protocol-compatibility.json)：8/8。
- [production-transforms.json](../../../artifacts/three-part-workspace-20261027/production-transforms.json)：43/43，包含 JPEG、PNG、WebP、TIFF、下载 hash、缩放/裁剪边界、幂等和原图不变。

生产服务重启后的 resume 也已通过：Web workspace 8/8：[production-web-restart.json](../../../artifacts/three-part-workspace-20261027/production-web-restart.json)；项目分组、历史、图像和标签 3/3：[production-groups-restart.json](../../../artifacts/three-part-workspace-20261027/production-groups-restart.json)。

生产切换与重启回执分别为 [production-apply-20261027T060222Z.json](../../../artifacts/production-prep-20261027/production-apply-20261027T060222Z.json) 和 [restart-receipt.json](../../../artifacts/production-prep-20261027/restart-receipt.json)。首轮 apply readiness 尚未满足时曾短暂出现恢复失败；随后完成修正并重启，当前 release health、`/app/`、数据库 migration 和重启后的复核均稳定通过。

生产切换的即时备份基线为 5,832 个对象，切换后仍为 5,832 个，added/removed/changed 均为 0；证据在 apply 回执的 `s3Baseline` 字段中。桌面本机原有 3 个 manifest 也保持不变：[existing-data-unchanged.json](../../../artifacts/three-part-workspace-20261027/existing-data-unchanged.json)。

## 打包和回滚边界

[package-receipt.json](../../../artifacts/three-part-workspace-20261027/package-receipt.json) 标记 `passed=true`：交付 app、验收 app 和挂载 DMG 的 file-backed Mach-O sections 均为 30，loaded-sections hash 一致，挂载 DMG 的 binary 与交付 app 匹配，且 `hdiutil verify` 通过。签名证据是 ad-hoc `codesign --verify --deep --strict`，不代表 notarization。

Migration 0009 已应用；按 [rollback-schema-compatibility.md](../../../artifacts/production-prep-20261027/rollback-schema-compatibility.md) 的 runbook，不得降级 schema，也不得使用旧 binary 写入项目分组。该限制用于保护新 `projectMetadata` 字段和旧客户端协议边界。

## 可复现命令

以下命令使用占位路径；请替换 native helper 地址、staging 地址、fixture 报告和私有 credentials 文件。credentials 只由脚本在内存中读取，不要写入命令行或报告。

```bash
python3 scripts/verify-dataset-groups.py \
  --native-base-url http://127.0.0.1:<native-fixture-port> \
  --output /tmp/native-groups.json

python3 scripts/verify-native-web-group-sync.py \
  --native-a-url http://127.0.0.1:<native-a-port> \
  --native-b-url http://127.0.0.1:<native-b-port> \
  --credentials /path/to/private-credentials.json \
  --local-report /tmp/native-groups.json \
  --base-url https://<staging-host> \
  --output artifacts/three-part-workspace-20261027/native-web-group-sync.json

python3 scripts/verify-data-project-protocol-compatibility.py \
  --credentials /path/to/private-credentials.json \
  --base-url https://<staging-host> \
  --output artifacts/three-part-workspace-20261027/protocol-compatibility.json
```

复用脚本：[verify-native-web-group-sync.py](../../../scripts/verify-native-web-group-sync.py) 和 [verify-data-project-protocol-compatibility.py](../../../scripts/verify-data-project-protocol-compatibility.py)。
