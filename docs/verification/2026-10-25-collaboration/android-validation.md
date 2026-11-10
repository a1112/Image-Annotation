# Android 最终构建验收（2026-10-25）

## 产物

- 本地 APK：[artifacts/mobile/image-annotation-arm64-debug-20261025.apk](../../../artifacts/mobile/image-annotation-arm64-debug-20261025.apk)
- SHA-256：`46cb78cf52b61cf5a559c0feeeed962f0098800ce05da0106a4a935615ae8eab`
- 大小：243,976,608 bytes
- 变体：`universalDebug`，native ABI `arm64-v8a`
- 应用 ID：`com.imageannotation.desktop`
- 版本：`0.1.0`（versionCode `1000`）
- min/compile/target SDK：24 / 35 / 35

构建在云端完成，源目录为：
`/home/ubuntu/projects/.deployment/image-annotation-android-20261025`

命令：

```text
CI=1 npx tauri android build --target aarch64 --apk --debug --ci
```

本次快照包含 iPad 桌面 User-Agent + 多点触控识别、任务页不请求审阅队列、移动端注册 `get_file_asset_path` handler、前台 native outbox 调度修复，以及移动标注工具栏主操作不压窄和属性抽屉 overlay 的移动 grid 定位修复及属性抽屉独立关闭按钮。构建前端资源为 `index-COhoVamC.js` 与 `index-DMz4VnXL.css`。

## 源快照核对

采用“按相对路径排序后，对每个文件计算 SHA-256，再对清单计算 SHA-256”的方式：

- source manifest：`3d22fa4f4783f5ecc48c3f38bf462a34df1db012aa766db63fdeb0ef4ee4de6b`（91 个共享客户端源文件）
- `dist` manifest：`03ab4d785c4d23738584bbb31b2ad84ee3369c98149feff5edc06d30a51e3e72`
- 本地与云端 source manifest：一致
- 本地 `dist` 与云端 Tauri `beforeBuildCommand` 生成的前端资源：一致

共享 source 清单覆盖 `src`、`src-tauri/src`、`src-tauri/capabilities`、前端/Cargo 清单与锁定文件、`src-tauri/build.rs` 和 `tauri.conf.json`；逐文件列表见 [client-source-manifest.json](client-source-manifest.json)。平台生成文件不混入共享源码哈希。最终 APK 内嵌 native library 中已核对新 JS/CSS 资源名；旧资源名不存在。

最终构建时间：2026-10-25 18:36:57 +08:00。

## 签名与 16KB 验证

- `apksigner verify --verbose`：通过
- APK Signature Scheme v2：通过
- 签名证书：`C=US, O=Android, CN=Android Debug`
- 证书 SHA-256：`a4a80a3c0172ba63a67d3680d449e72dba8c73a65d7539a867d4a39371752ff4`
- `zipalign -c -P 16 -v 4`：`Verification successful`
- `lib/arm64-v8a/libimage_annotation_lib.so` 的 4 个 ELF `LOAD` 段：均为 `0x4000`
- native ELF debug section：0（构建使用 `-C debuginfo=0 -C strip=debuginfo`）

## 设备验收

当前没有连接 Android 真机，按用户选择暂缓实机安装/交互验收；APK 可用下列命令安装：

```bash
adb install -r artifacts/mobile/image-annotation-arm64-debug-20261025.apk
```

完整机器可复核数据见 [`artifacts/mobile/build-receipt.json`](../../../artifacts/mobile/build-receipt.json)。
