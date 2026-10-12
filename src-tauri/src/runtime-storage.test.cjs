// Structural integration checks supplement (not replace) the exact Rust tests.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const source = (file) => fs.readFileSync(path.join(__dirname, file), "utf8");

test("storage selection gates background sync and HTTP startup", () => {
  const lib = source("lib.rs");
  const setup = lib.slice(lib.indexOf("app = app.setup(|app|"));
  assert.ok(setup.indexOf("storage_setup::initialize(app.handle())") < setup.indexOf("sync_engine::start_auto_sync()"));
  assert.ok(setup.indexOf("storage_setup::initialize(app.handle())") < setup.indexOf("http_backend::start_background_backend"));
  assert.match(setup, /storage_setup::initialize\(app.handle\(\)\)\.map_err\(std::io::Error::other\)\?/);
});

test("packaged storage uses the Tauri per-user path, including debug bundles", () => {
  const setup = source("storage_setup.rs");
  assert.match(setup, /app\.path\(\)\.app_data_dir\(\)/);
  assert.match(setup, /#\[cfg\(all\(dev, debug_assertions\)\)\]/);
  assert.ok(setup.includes("#[cfg(not(all(dev, debug_assertions)))]"));
  assert.match(setup, /runtime_storage::resolve_root\(&app_data, &legacy_root/);
});

test("release project paths cannot default to a compile-time repository or working directory", () => {
  const project = source("project_fs.rs");
  const root = project.slice(project.indexOf("pub fn workspace_root()"), project.indexOf("#[cfg(test)]\npub fn workspace_root()"));
  const release = root.slice(root.indexOf("#[cfg(not(all(dev, debug_assertions)))]"));
  assert.match(release, /STORAGE_ROOT/);
  assert.doesNotMatch(release, /CARGO_MANIFEST_DIR|current_dir|development_workspace_root/);
  assert.match(source("http_backend.rs"), /run_foreground_backend_at\(None\)/);
  assert.match(source("http_backend.rs"), /project_fs::initialize_development_storage\(\)\?/);
  assert.match(source("../examples/image-annotation-backend.rs"), /parse_data_root_args/);
});

test("runtime health supplies the actual selected workspace location", () => {
  const http = source("http_backend.rs");
  assert.match(http, /"workspaceDataRoot": project_fs::workspace_data_root\(\)/);
});

test("external linked datasets retain their chosen image root and existing layout", () => {
  const datasets = source("datasets.rs");
  assert.match(datasets, /source_dataset_key: "local-linked"\.to_string\(\)/);
  assert.match(datasets, /root_path: canonical\.to_string_lossy\(\)\.to_string\(\)/);
  const project = source("project_fs.rs");
  assert.match(project, /workspace_root\(\)\.join\("data"\)\.join\("test_data"\)/);
  assert.match(project, /\.join\("workspaces"\)\s*\.join\("default"\)/);
});

test("no frontend requests can run before storage selection", () => {
  const config = JSON.parse(source("../tauri.conf.json"));
  assert.equal(config.app.windows.find((window) => window.label === "main").create, false);
  const setup = source("lib.rs").slice(source("lib.rs").indexOf("app = app.setup(|app|"));
  assert.ok(setup.indexOf("storage_setup::initialize") < setup.indexOf("WebviewWindowBuilder::from_config"));
});

test("mobile setup never instantiates the desktop storage dialog", () => {
  const setup = source("storage_setup.rs");
  assert.ok(setup.includes("#[cfg(all(not(mobile), not(all(dev, debug_assertions))))]\nstruct NativeStorageUi"));
  assert.match(setup, /#\[cfg\(mobile\)\][\s\S]*read_selection/);
});
