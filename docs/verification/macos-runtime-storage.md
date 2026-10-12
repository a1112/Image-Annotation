# Packaged runtime storage ownership

## Behavior

Packaged desktop builds obtain the current user's application data directory from
Tauri's `app.path().app_data_dir()` (`com.imageannotation.desktop`). They do not
use the build machine, installation bundle, repository, or working directory as
an implicit write location. Debug bundles follow this policy too; only a
non-bundled debug development build (`cfg(dev)` and `debug_assertions`) retains
the existing repository layout.

Before the desktop frontend or background data services start, a first launch
without a saved selection asks the user to choose:

- The per-user application data location, recommended for a new installation.
- An existing work directory containing `data/workspaces/default` or
  `data/test_data`.
- Cancel and exit without starting data services.

The prompt explains that a new location does not migrate old projects. It shows
legacy data at the embedded historical repository location when detectable, and
also explains that other older builds may have used a different location.
Access errors inspecting legacy data are shown, rather than treated as absence.
No automatic move, copy, manifest rewrite, delete, administrator escalation, or
Full Disk Access request is made.

An explicit choice is saved atomically as `storage-root.txt` under the per-user
app data directory. Existing selections reopen without another prompt. A
missing/unmounted/unreadable/unwritable selection does not become a replacement
empty workspace: the user can retry, explicitly select a location again, or
exit. The write check uses new temporary files and never truncates project
files. A config-write failure preserves the prior selection.

The storage format is unchanged. Managed projects still use
`data/workspaces/default/projects`, and built-in datasets/downloads use
`data/test_data`. Existing `local-linked` image paths remain selected in place.
The project information dialog displays the actual workspace location reported
by the backend, with an explicit unknown state for older backends.

Mobile setup uses its platform-owned app data directory without constructing
any desktop storage dialogs. An existing saved selection still cannot silently
fall back.

## Standalone development backend

The ordinary `npm run dev` / `cargo run --example image-annotation-backend`
workflow continues to use the repository's existing data directory. A release
standalone launch requires an explicit absolute root:

```sh
cargo run --release --manifest-path src-tauri/Cargo.toml \
  --example image-annotation-backend -- --data-root /absolute/work-directory
```

The selected root contains the same `data/...` layout. Missing directories are
created only at this explicit root; existing data is not migrated. Relative,
missing, unknown, or extra arguments are rejected. A release launch without
`--data-root` reports how to select storage and never falls back to the builder's
repository. Desktop launches use the native selection flow instead of this CLI.

## Automated verification (2026-10-12, Linux)

- 20 exact `runtime_storage.rs` Rust tests compiled with Rust 1.89.0, using only
  temporary directories. This includes real permission-denied reads/writes as
  UID 1000, explicit first-run choices, cancellation, saved-root retry/reselection,
  missing volumes, no legacy modification, atomic config replacement failure,
  previous-config preservation, write-probe cleanup, and CLI argument handling.
- 118 frontend Vitest tests, including actual per-user/selected workspace path
  presentation and the unknown-location state.
- 12 existing window-chrome Node tests.
- 7 Node structural integration checks for startup ordering, delayed frontend
  creation, release/dev separation, mobile dialog exclusion, selected external
  paths, and health location reporting.
- TypeScript and Vite production build.

To run the isolated filesystem policy without Tauri/native dependencies:

```sh
rustc --edition 2021 --test src-tauri/src/runtime_storage.rs -o /tmp/image-storage-tests
/tmp/image-storage-tests
npm test
npm run build
```

Permission-denial tests require an unprivileged account. No real datasets,
credentials, user projects, or macOS privacy settings were used in these tests.

## Native release checks still required

This is not a native macOS/Tauri build or runtime certification. On a signed
macOS release (and a debug bundle), verify:

1. Install outside the checkout; launch as a non-admin user with the compile-time
   repository absent/read-only. Confirm the first-run choice precedes the UI and
   the recommended location is in that user's Application Support directory.
2. Cancel both the choice and folder picker; verify no implicit new workspace,
   background download, or sync starts. Repeat the choice and relaunch.
3. Select a copied legacy fixture; compare manifests/SQLite/assets before and
   after opening. Verify external-linked image paths stay unchanged.
4. Remove or deny access to the saved root, then retry, reselect, and cancel.
   Confirm errors are clear and the previous selection remains intact if saving
   a replacement config fails.
5. Create/annotate/export a temporary project and restart. Verify all managed
   writes stay beneath the selected root, thumbnails/assets render, and window,
   tray, and annotation-window behavior remains intact.
6. Smoke-test the development and explicit-root standalone backend workflows;
   compile-check mobile separately for the platform-only startup branch.

API references: [Tauri dev-mode detection](https://docs.rs/tauri-build/latest/tauri_build/fn.is_dev.html),
[Tauri delayed window creation](https://docs.rs/tauri-utils/latest/tauri_utils/config/struct.WindowConfig.html#structfield.create),
[Tauri 2.11.1 window builder](https://docs.rs/tauri/2.11.1/tauri/webview/struct.WebviewWindowBuilder.html#method.from_config),
[rfd 0.15.4 message buttons](https://docs.rs/rfd/0.15.4/rfd/enum.MessageButtons.html).
