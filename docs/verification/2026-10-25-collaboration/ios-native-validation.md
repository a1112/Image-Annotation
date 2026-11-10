# iOS native validation (2026-10-25)

This note records the local build and simulator evidence for the mobile
runtime. It intentionally does not contain account passwords or tokens.

## Source and outputs

- Source checkout at validation: `8731402` plus the uncommitted collaboration
  changes in this worktree.
- The generated Xcode project and `src-tauri/tauri.ios.conf.json` use the
  certificate's actual Team Identifier: `VKFR723PW9`. The value in the
  certificate display name, `3256NJ72B9`, is the developer display/account
  suffix and is not the Team Identifier.
- Release, arm64 simulator, entitlement-aware app:
  `artifacts/ios-simulator/app/Image Annotation.app` (built at 18:41).
  Main executable SHA-256:
  `a4afba4806b6cd89e9c4f3eadf9a90580a9b4b20e7aaa2b55cc418ab7dee43f5`.
- Release, arm64 device, unsigned IPA fallback:
  `artifacts/ios-device/Image Annotation-unsigned.ipa` (built at 18:42,
  SHA-256 `74e27c26bf144db81c89c6cac9eab0fc5a7f1b9a129b96873bea6f1cd43b5cbb`).
  The matching receipt and the signed-build failure evidence are in
  `artifacts/ios-device/build-receipt.json`.
- The frontend dist manifest is
  `03ab4d785c4d23738584bbb31b2ad84ee3369c98149feff5edc06d30a51e3e72` in both
  mobile outputs. The embedded frontend assets are `index-COhoVamC.js` and
  `index-DMz4VnXL.css`; the final frontend gate passed 161 Vitest tests and
  12 Chrome tests, including the annotator review-queue permission fix and
  touch-capable iPad User-Agent detection, and foreground sync scheduling.
- The iOS receipts use the final 91-file client source manifest at
  `docs/verification/2026-10-25-collaboration/client-source-manifest.json`,
  whose manifest hash is
  `3d22fa4f4783f5ecc48c3f38bf462a34df1db012aa766db63fdeb0ef4ee4de6b`.
  The Android receipt uses the same shared client source scope. The client
  manifest includes the drawer close-button source and the frozen native
  bridge files, and its listed per-file hashes reproduce the recorded value.
- The generated Xcode project is reproducible with
  `scripts/build-ios-simulator.sh`. The script defaults to
  `IMAGE_ANNOTATION_REBUILD_RUST=1`, so it regenerates the Rust archive before
  the final Xcode entitlement pass. Set it explicitly to `0` only for a
  packaging-only rerun when the archive is already known to be current; the
  default output is under `artifacts/`.

The mobile Tauri handler now registers `get_file_asset_path` alongside the
other mobile image commands. The existing asset protocol remains enabled and
its scope includes the mobile app data/cache paths (`**` plus the project data
scopes), so `convertFileSrc` can read the resolved local image path.

The device development-signing attempt was run with the configured team and
failed before compilation because Xcode rejected account `8615581217192` and
could not find a development profile for `com.imageannotation.desktop` (exit
65). The full log is `/tmp/image-annotation-ios-device-final.log`. A fresh
`--no-sign` release build completed from the final source at 18:42; its build
log is `/tmp/image-annotation-ios-drawer-device-build.log`, and the IPA is
valid as an archive but must be signed with a real development
certificate/profile before it can install on a device. Device installation
was intentionally deferred for this pass.

## Simulator install and visual evidence

- Simulator: iPhone 17 Pro Max, iOS 26.5, UDID
  `782FB55D-8079-4478-80AA-7EE3F4571087`.
- The final entitlement-aware simulator app was installed with `simctl` on
  iPhone 17 Pro Max (`782FB55D-8079-4478-80AA-7EE3F4571087`) and iPad Air
  11-inch (M4) (`5927DF91-C3B3-44B6-A110-1A5EBD05B064`) and both were
  terminated, reinstalled from the final app, and launched without click
  interaction. The iPhone screenshot is
  `artifacts/ios-simulator/drawer-close-final-launch.png`; the iPad screenshot
  is `artifacts/ios-simulator/drawer-close-final-ipad-launch.png`. Both show
  the final responsive mobile project shell with the three non-empty image
  thumbnails and zero pending reviews after the approve5 flow; the updated
  toolbar and drawer layout no longer render the desktop inspector over this
  mobile route. The receipt records both new bundle-container paths and launch
  PIDs.

The screenshots show the mobile project/task/review shell, an online remote
project, and non-empty image thumbnails. No user credentials are present in
the screenshots.

## Keychain signing evidence

The available identity is:

`Apple Development: 8615581217192 (3256NJ72B9)`

`codesign -dvvv` on the available identity reports
`TeamIdentifier=VKFR723PW9`. A copy signed after the build with this identity
and no simulator xcent launches, but iOS `securityd` returns `-34018: Client
has neither application-identifier nor keychain-access-groups entitlements`
when the protected Keychain store is read. This is visible in the screenshot's
native-session error and in the simulator log:

```text
SecError ... Code=-34018 "Client has neither application-identifier nor keychain-access-groups entitlements"
```

The working simulator path is to pass the entitlements to `xcodebuild` before
signing. The command-line overrides are:

```text
CODE_SIGN_IDENTITY=- CODE_SIGNING_ALLOWED=YES CODE_SIGNING_REQUIRED=NO
AD_HOC_CODE_SIGNING_ALLOWED=YES CODE_SIGN_INJECT_BASE_ENTITLEMENTS=YES
ENTITLEMENTS_ALLOWED=YES PROVISIONING_PROFILE_REQUIRED=NO
DEVELOPMENT_TEAM=VKFR723PW9
```

The resulting Xcode-generated `Image Annotation.app-Simulated.xcent` contains
`application-identifier=VKFR723PW9.com.imageannotation.desktop`,
`com.apple.developer.team-identifier=VKFR723PW9`, `get-task-allow=true`, and
the matching Keychain group. The successful run reports the native device ID
Keychain read/write path working in the simulator; no provisioning profile is
required for this simulator build. Do not post-sign the app, because that
discards the simulator xcent path that supplies these entitlements.

The device IPA remains a separate release/no-sign artifact pending real-device
development provisioning. No plaintext credential fallback was added; the
Rust mobile credential store remains the iOS protected Keychain store.


## Final GUI acceptance

After the final install, the root agent operated the app through DeviceHub:

- iPhone retained the native login, opened the approved five-object annotation,
  closed the inspector with its own close button, and rotated between portrait
  and landscape without losing objects. The final GUI offline-pin action
  persisted one original as pinned; all three original hashes match.
- iPad displayed the five-object approved annotation in landscape with a right
  inspector sidebar. Before the final close-button-only rebuild, actual GUI
  review approval targeted submitted annotation revision 5 and produced
  approved revision 6.
- Desktop GUI then displayed 通过 and five objects after image navigation
  refreshed its loaded canvas.

See [mobile-ui-validation.json](mobile-ui-validation.json),
[cross-device-server-validation.json](cross-device-server-validation.json),
[phone portrait](iphone-approved-final.png),
[phone inspector](iphone-inspector-open.png),
[phone landscape](iphone-landscape.png),
[offline pin](iphone-offline-pin.png),
[iPad approval](ipad-approved.png),
[iPad landscape](ipad-landscape-approved.png), and
[desktop approved result](desktop-approved-five-boxes.png).

These are simulator interactions. Physical multi-touch and the two-device
network-disconnect GUI scenario were not performed; offline conflict/restart
behavior was exercised separately against the real server and two SQLite
clients. Android hardware and iOS signing/install remain deferred per user.
