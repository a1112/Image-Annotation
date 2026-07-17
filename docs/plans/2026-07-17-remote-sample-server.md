# Remote Sample Server Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build and verify a standalone authenticated service for remotely managing dataset projects, samples, assets, annotations, imports, review state, and recoverable deletion.

**Architecture:** Add an Axum-based binary alongside the Tauri application. The service reuses the existing domain and format adapters, selects a configurable workspace data root at process startup, exposes an explicit versioned REST API, and keeps remote import/trash/audit state in a service SQLite database.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, Tower HTTP, Serde, rusqlite, reqwest, existing image/ZIP/import adapters, React/TypeScript transport profiles, Docker Compose.

---

### Task 1: Configurable Runtime Data Root

**Files:**
- Modify: `src-tauri/src/project_fs.rs`
- Modify: `src-tauri/tests/real_datasets.rs`

**Step 1: Write the failing tests**

Add pure configuration tests that prove:

```rust
#[test]
fn configured_workspace_root_is_independent_of_source_checkout() {
    let root = PathBuf::from("D:/sample-server-data");
    assert_eq!(
        workspace_data_root_from(Some(root.clone()), Path::new("F:/source")),
        root
    );
}

#[test]
fn default_workspace_root_preserves_desktop_layout() {
    assert_eq!(
        workspace_data_root_from(None, Path::new("F:/source")),
        Path::new("F:/source/data/workspaces/default")
    );
}
```

**Step 2: Run the tests and verify RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_remote_target'
cargo test --manifest-path src-tauri\Cargo.toml workspace_root -- --nocapture
```

Expected: compilation fails because `workspace_data_root_from` does not exist.

**Step 3: Implement the runtime layout**

Add:

```rust
pub fn workspace_data_root_from(configured: Option<PathBuf>, checkout_root: &Path) -> PathBuf
pub fn configure_workspace_data_root(path: PathBuf) -> Result<(), String>
```

Use a process-level `OnceLock<PathBuf>`. `workspace_data_root()` checks the configured
root, then `IMAGE_ANNOTATION_DATA_DIR`, then the existing checkout-relative default.
Validate that the configured path is non-empty and create it during server startup.

Do not change test-data fixture paths.

**Step 4: Run tests and verify GREEN**

Run the targeted tests, then:

```powershell
cargo test --manifest-path src-tauri\Cargo.toml -- --test-threads=1
```

Expected: all existing tests remain green.

**Step 5: Commit**

```powershell
git add src-tauri/src/project_fs.rs src-tauri/tests/real_datasets.rs
git commit -m "feat: configure runtime workspace data root"
```

### Task 2: Server Configuration, Authentication, And Error Contract

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Modify: `src-tauri/src/lib.rs`
- Create: `src-tauri/src/remote_server/mod.rs`
- Create: `src-tauri/src/remote_server/config.rs`
- Create: `src-tauri/src/remote_server/auth.rs`
- Create: `src-tauri/src/remote_server/error.rs`
- Create: `src-tauri/src/bin/image-annotation-server.rs`
- Create: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing configuration and router tests**

Cover:

```rust
#[test]
fn non_loopback_bind_requires_a_token()

#[test]
fn role_order_is_reader_editor_admin()

#[tokio::test]
async fn health_is_public_but_projects_require_bearer_token()

#[tokio::test]
async fn reader_cannot_call_admin_route()
```

Assert the error envelope includes `error.code` and `requestId`, with `401` for missing
credentials and `403` for insufficient role.

**Step 2: Run tests and verify RED**

Run:

```powershell
cargo test --manifest-path src-tauri\Cargo.toml --test remote_server -- --nocapture
```

Expected: compilation fails because the remote server module and binary do not exist.

**Step 3: Add dependencies and minimal server**

Add direct dependencies:

```toml
axum = { version = "0.8", features = ["macros", "multipart"] }
clap = { version = "4", features = ["derive", "env"] }
http-body-util = "0.1"
sha2 = "0.10"
subtle = "2.6"
tokio = { version = "1", features = ["fs", "macros", "net", "rt-multi-thread", "signal"] }
tower = { version = "0.5", features = ["util"] }
tower-http = { version = "0.6", features = ["cors", "limit", "request-id", "trace"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

Implement:

- `ServerConfig::parse()` and `ServerConfig::validate()`;
- `Role::{Reader, Editor, Admin}`;
- constant-time token lookup with `subtle::ConstantTimeEq`;
- request authentication middleware;
- `ApiError` and response envelope types;
- public `GET /api/v1/health`;
- protected placeholder `GET /api/v1/projects`;
- `run(config)` and graceful shutdown in the binary.

Do not log token values.

**Step 4: Run tests and verify GREEN**

Run the remote server tests and `cargo check --bins`.

**Step 5: Commit**

```powershell
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs src-tauri/src/remote_server src-tauri/src/bin src-tauri/tests/remote_server.rs
git commit -m "feat: add authenticated remote server"
```

### Task 3: Service Database And Project Management

**Files:**
- Create: `src-tauri/src/remote_server/storage.rs`
- Create: `src-tauri/src/remote_server/service.rs`
- Create: `src-tauri/src/remote_server/projects.rs`
- Modify: `src-tauri/src/remote_server/mod.rs`
- Modify: `src-tauri/src/remote_server/error.rs`
- Modify: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing project lifecycle tests**

Use a temporary service root and real router requests:

```rust
POST /api/v1/projects
GET  /api/v1/projects
GET  /api/v1/projects/{id}
PATCH /api/v1/projects/{id}
DELETE /api/v1/projects/{id}
POST /api/v1/projects/{id}/restore
```

Assert:

- admin can create and rename a project;
- duplicate IDs return `409`;
- reader sees active projects;
- delete removes the project from normal listing and moves it under `trash/projects`;
- repeated delete is idempotent;
- restore returns the same project and files;
- editor cannot delete a project.

**Step 2: Run tests and verify RED**

Expected: project routes return `404`.

**Step 3: Implement service storage and routes**

Create `server.sqlite` with:

```sql
CREATE TABLE service_audit (...);
CREATE TABLE trashed_projects (...);
CREATE TABLE import_sessions (...);
```

Add a `RemoteSampleService` that coordinates filesystem and SQLite operations. Use the
existing `datasets::create_dataset_project` and `SampleRepository` for active projects.
Sanitize project names and IDs. Move project directories only within configured data
and trash roots.

Map not-found and conflict errors to stable API codes.

**Step 4: Run tests and verify GREEN**

Run project lifecycle tests and the full Rust suite.

**Step 5: Commit**

```powershell
git add src-tauri/src/remote_server src-tauri/tests/remote_server.rs
git commit -m "feat: manage remote dataset projects"
```

### Task 4: Sample Query, Metadata, Assets, And Class Filtering

**Files:**
- Create: `src-tauri/src/remote_server/samples.rs`
- Modify: `src-tauri/src/remote_server/service.rs`
- Modify: `src-tauri/src/remote_server/mod.rs`
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/storage.rs`
- Modify: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing sample API tests**

Create a three-image fixture with two classes and test:

```text
GET /projects/{id}/samples?offset=0&limit=2
GET /projects/{id}/samples?split=train
GET /projects/{id}/samples?classId=1
GET /projects/{id}/samples?status=待质检
GET /projects/{id}/samples?q=demo_001
GET /projects/{id}/samples/{imageId}
PATCH /projects/{id}/samples/{imageId}
GET /projects/{id}/samples/{imageId}/content
GET /projects/{id}/samples/{imageId}/thumbnail
```

Assert pagination includes an accurate `total`, class filtering checks annotation
objects, metadata patch validates split/status values, asset bytes have the correct
content type and ETag, `If-None-Match` returns `304`, and valid byte ranges return `206`.

**Step 2: Run tests and verify RED**

Expected: sample routes return `404`.

**Step 3: Implement query and asset handlers**

Add structured `SampleQuery` and `Page<T>` types. Apply cheap SQLite filters before
annotation class filtering. Add storage methods for metadata updates and counts.

Resolve assets only through indexed project/image IDs. Generate missing thumbnails on
demand into the existing thumbnail directory. Compute ETag from file metadata and SHA-256
for small generated thumbnails. Parse one RFC 7233 byte range and reject invalid ranges.

**Step 4: Run tests and verify GREEN**

Run targeted tests, full Rust tests, and `cargo clippy --all-targets -- -D warnings`.

**Step 5: Commit**

```powershell
git add src-tauri/src/remote_server src-tauri/src/domain.rs src-tauri/src/storage.rs src-tauri/tests/remote_server.rs
git commit -m "feat: query and stream remote samples"
```

### Task 5: Annotation Revision, History, Submit, And Review API

**Files:**
- Create: `src-tauri/src/remote_server/annotations.rs`
- Modify: `src-tauri/src/remote_server/mod.rs`
- Modify: `src-tauri/src/remote_server/error.rs`
- Modify: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing annotation workflow tests**

Exercise:

```text
GET  /samples/{imageId}/annotations
PUT  /samples/{imageId}/annotations
GET  /samples/{imageId}/annotations/history
POST /samples/{imageId}/submit
POST /samples/{imageId}/review
```

Assert:

- reads return revision and ETag;
- first save without revision succeeds;
- second save with matching `If-Match` succeeds;
- stale `If-Match` returns `409 revision_conflict`;
- history contains both revisions;
- submit changes status to pending review;
- editor can submit and review, reader cannot mutate;
- invalid class IDs and geometry return `422`.

**Step 2: Run tests and verify RED**

Expected: annotation routes return `404`.

**Step 3: Implement handlers and validation**

Reuse:

```rust
SampleRepository::image_annotation_state
SampleRepository::save_image_annotations_with_revision
SampleRepository::annotation_history
SampleRepository::submit_image_annotations
SampleRepository::review_task_item
```

Normalize quoted `If-Match` values. Validate bbox dimensions, polygon point count,
finite coordinates, and class membership before saving. Translate stale revisions to
`409` without exposing SQLite errors.

**Step 4: Run tests and verify GREEN**

Run targeted and full Rust suites.

**Step 5: Commit**

```powershell
git add src-tauri/src/remote_server src-tauri/tests/remote_server.rs
git commit -m "feat: edit and review remote annotations"
```

### Task 6: Streaming Upload, Analysis, And Confirmed Import

**Files:**
- Create: `src-tauri/src/remote_server/imports.rs`
- Modify: `src-tauri/src/remote_server/storage.rs`
- Modify: `src-tauri/src/remote_server/service.rs`
- Modify: `src-tauri/src/remote_server/mod.rs`
- Modify: `src-tauri/src/datasets.rs`
- Modify: `src-tauri/src/project_fs.rs`
- Modify: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing upload transaction tests**

Create in-memory ZIP and multipart fixtures. Assert:

- editor can upload a valid YOLO/VOC/COCO/LabelMe fixture;
- upload returns an import ID, detected format, tree, counts, and `analyzed` state;
- no project samples appear before commit;
- commit with explicit format copies and indexes data;
- cancelling removes staging files;
- parent traversal ZIP entries are rejected;
- disallowed extensions, file-count excess, and byte-limit excess are rejected;
- a client-supplied server path is never accepted.

**Step 2: Run tests and verify RED**

Expected: imports route returns `404`.

**Step 3: Implement staged imports**

Implement multipart streaming with `Multipart::next_field()` and `Field::chunk()`.
Write to:

```text
{data-root}/staging/imports/{import-id}/payload
```

Extract ZIP archives with `safe_extract_path`, rejecting links and non-regular entries.
Run `datasets::analyze_data_source_with_override` against the staged payload.

Add:

```rust
datasets::import_staged_dataset_into_project(project_id, staged_root, format)
```

This copies staged content into the project raw root, indexes it using the chosen
adapter, records source mappings, and never leaves a project linked to staging.

Persist each state transition in `server.sqlite`. Publish only after successful indexing;
retain a failed session for inspection and allow cancellation.

Apply `DefaultBodyLimit` only to the upload route and enforce the configured streaming
limit independently.

**Step 4: Run tests and verify GREEN**

Run import tests, all format adapter tests, and the full Rust suite.

**Step 5: Commit**

```powershell
git add src-tauri/src/remote_server src-tauri/src/datasets.rs src-tauri/src/project_fs.rs src-tauri/tests/remote_server.rs
git commit -m "feat: upload and confirm remote dataset imports"
```

### Task 7: Recoverable Sample Deletion And Restore

**Files:**
- Modify: `src-tauri/src/storage.rs`
- Modify: `src-tauri/src/remote_server/storage.rs`
- Modify: `src-tauri/src/remote_server/service.rs`
- Modify: `src-tauri/src/remote_server/samples.rs`
- Modify: `src-tauri/tests/remote_server.rs`

**Step 1: Write failing trash tests**

Assert:

- admin can delete a sample;
- image, thumbnail, native annotation, and linked-format sidecar move into trash;
- normal listings and counts exclude the sample;
- annotations and content return `404` while trashed;
- repeated delete is idempotent;
- restore returns all files and metadata;
- restore detects occupied target paths and returns `409`;
- editor cannot delete or restore.

**Step 2: Run tests and verify RED**

Expected: `DELETE` and restore routes are unavailable.

**Step 3: Implement project-local trash metadata**

Add a `sample_trash` table to each project database. Store original relative paths,
trash relative paths, and timestamp. Change image queries to exclude rows present in
`sample_trash`.

Coordinate filesystem moves using a pending trash directory and database transaction.
Rollback file moves when metadata persistence fails. Restore is the inverse operation.

**Step 4: Run tests and verify GREEN**

Run trash tests and full Rust regression.

**Step 5: Commit**

```powershell
git add src-tauri/src/storage.rs src-tauri/src/remote_server src-tauri/tests/remote_server.rs
git commit -m "feat: trash and restore remote samples"
```

### Task 8: Desktop Remote Backend Profile

**Files:**
- Create: `src/api/backend-profile.ts`
- Create: `src/api/backend-profile.test.ts`
- Modify: `src/api/tauri.ts`
- Modify: `src/api/tauri.test.ts`
- Modify: `src/types/domain.ts`
- Modify: `src/App.tsx`
- Modify: `src/App.test.tsx`
- Modify: `src/styles.css`

**Step 1: Write failing transport tests**

Assert:

- local profile still invokes Tauri and falls back to loopback HTTP;
- remote profile uses the configured `/api/v1` URL and Bearer token;
- remote project, sample, asset, annotation, and import calls map to REST resources;
- a `409` response becomes a typed revision conflict;
- token is not written to `localStorage`;
- disconnect removes the in-memory token.

Add UI tests for opening the connection dialog, checking health, showing role, activating
remote mode, and disconnecting.

**Step 2: Run tests and verify RED**

Run:

```powershell
npm test -- --run src/api/backend-profile.test.ts src/api/tauri.test.ts src/App.test.tsx
```

Expected: missing profile module and controls.

**Step 3: Implement transport and connection UI**

Create a memory-only profile store. Add an explicit remote REST transport while keeping
the existing local command transport unchanged. Expose a compact connection dialog from
the settings area using existing controls and Lucide icons.

Disable desktop-only folder picker actions in remote mode and route remote import
through file upload and confirmation.

**Step 4: Run tests and verify GREEN**

Run targeted tests, all frontend tests, and `npm run build`.

**Step 5: Commit**

```powershell
git add src/api src/types/domain.ts src/App.tsx src/App.test.tsx src/styles.css
git commit -m "feat: connect desktop client to remote samples"
```

### Task 9: Deployment Artifacts And Operations Documentation

**Files:**
- Create: `Dockerfile.server`
- Create: `docker-compose.server.yml`
- Create: `.env.server.example`
- Create: `deploy/Caddyfile.example`
- Create: `docs/remote-server.md`
- Modify: `README.md`

**Step 1: Write failing artifact contract test**

Add a Rust or Node test that parses the Compose YAML text and asserts:

- the server command is present;
- port `17311` is published;
- a persistent data volume maps to `/data`;
- admin token and allowed origins are environment-driven;
- the health check calls `/api/v1/health`;
- no token literal is committed.

**Step 2: Run test and verify RED**

Expected: files are missing.

**Step 3: Add deployment artifacts**

Use a multi-stage Rust build. Run as a non-root user, expose `17311`, mount `/data`, and
set a read-only root filesystem where supported. Document Windows binary startup,
Docker Compose startup, token generation, Caddy HTTPS proxying, backup of the data root,
and API examples.

**Step 4: Validate**

Run:

```powershell
docker compose -f docker-compose.server.yml config
```

when Docker is available. Always run the artifact contract test.

**Step 5: Commit**

```powershell
git add Dockerfile.server docker-compose.server.yml .env.server.example deploy docs/remote-server.md README.md
git commit -m "docs: deploy remote sample server"
```

### Task 10: Live Server End-To-End Verification

**Files:**
- Create: `src-tauri/tests/remote_server_live.rs`
- Modify: `scripts/dev-with-backend.mjs`
- Modify: `package.json`

**Step 1: Write the live workflow test**

Start the Axum server on `127.0.0.1:0` with a temporary data root and real tokens. Use
`reqwest` over TCP to verify:

1. health without authentication;
2. unauthorized project list;
3. admin project creation;
4. editor multipart upload and analysis;
5. import confirmation;
6. class-filtered sample listing;
7. asset and thumbnail download;
8. annotation first save and matching revision save;
9. stale revision conflict;
10. submit and review;
11. sample delete and restore;
12. project delete and restore.

**Step 2: Run test and verify RED**

Expected: any incomplete endpoint fails the workflow.

**Step 3: Complete lifecycle and shutdown behavior**

Expose a listener-based `serve(listener, config, shutdown)` function for tests. Ensure
graceful shutdown stops accepting requests and waits for in-flight mutations.

Add scripts:

```json
"server:dev": "cargo run --manifest-path src-tauri/Cargo.toml --bin image-annotation-server --",
"server:check": "cargo test --manifest-path src-tauri/Cargo.toml --test remote_server_live"
```

**Step 4: Run final verification**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_remote_target'
cargo fmt --manifest-path src-tauri\Cargo.toml -- --check
cargo clippy --manifest-path src-tauri\Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri\Cargo.toml -- --test-threads=1
npm test -- --run
npm run build
git diff --check
```

Start the binary with temporary data and a generated token, then call health and one
authenticated endpoint from PowerShell. Stop the process before completion.

**Step 5: Commit**

```powershell
git add src-tauri/tests/remote_server_live.rs scripts/dev-with-backend.mjs package.json
git commit -m "test: verify remote sample server workflow"
```

## Completion Audit

Before marking the goal complete, verify each confirmed design requirement against
authoritative evidence:

- standalone process starts without Tauri;
- configurable data root is used;
- non-loopback bind without token is rejected;
- reader/editor/admin permissions are enforced;
- project lifecycle is remotely manageable;
- sample filtering, metadata, content, and thumbnail endpoints work;
- uploads are staged, analyzed, confirmed, limited, and path-safe;
- all supported annotation formats import through the remote flow;
- annotations use revision conflict control and history;
- submit and review workflows work;
- sample and project deletion are recoverable;
- desktop client can connect and disconnect without persisting the token;
- deployment artifacts validate;
- live TCP end-to-end workflow passes;
- all pre-existing tests remain green.
