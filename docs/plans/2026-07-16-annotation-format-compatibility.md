# Annotation Format Compatibility Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add reliable dataset inspection, import, source synchronization, and export for YOLO Detect/Seg, Pascal VOC, COCO, LabelMe, and image-only directories.

**Architecture:** Introduce a canonical source-mapping model in SQLite and a registry of format adapters. Adapters parse and write external files, while the existing dataset repository remains responsible for projects, images, classes, canonical annotations, revisions, snapshots, tasks, and exports.

**Tech Stack:** Rust, serde/serde_json, quick-xml, rusqlite, image, walkdir, Tauri v2, React, TypeScript, Vitest, Testing Library.

---

### Task 1: Canonical Source Mapping And Inspection Problems

**Files:**
- Modify: `src-tauri/src/project_fs.rs`
- Modify: `src-tauri/src/storage.rs`
- Modify: `src-tauri/src/datasets.rs`
- Modify: `src-tauri/src/domain.rs`
- Modify: `src/types/domain.ts`
- Test: `src-tauri/src/storage.rs`
- Test: `src-tauri/src/datasets.rs`

**Step 1: Write the failing storage tests**

Add a test that initializes a project database and persists:

```rust
let source = StoredDatasetSource {
    format: "coco".to_string(),
    mode: "linked".to_string(),
    root_path: "L:/dataset".to_string(),
    annotation_path: Some("L:/dataset/annotations.json".to_string()),
    options_json: "{}".to_string(),
};
let mapping = StoredImageSource {
    image_id: "img-1".to_string(),
    relative_path: "images/a.jpg".to_string(),
    external_id: Some("42".to_string()),
    annotation_path: Some("annotations.json".to_string()),
    source_version: "100:1234".to_string(),
};
```

Assert both records round-trip through SQLite.

Add a dataset inspection serialization test for:

```rust
InspectionProblem {
    severity: "error".to_string(),
    code: "missing-image".to_string(),
    path: Some("images/a.jpg".to_string()),
    record: Some("42".to_string()),
    message: "COCO image file does not exist".to_string(),
}
```

**Step 2: Run tests to verify they fail**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml source_mapping
cargo test --manifest-path src-tauri\Cargo.toml inspection_problem
```

Expected: FAIL because source mapping tables and inspection problem fields do not exist.

**Step 3: Implement the schema and types**

Add `source_format`, `source_mode`, `source_annotation_path`, and
`source_options_json` to the project model. Add a `image_sources` table:

```sql
CREATE TABLE IF NOT EXISTS image_sources (
    image_id TEXT PRIMARY KEY,
    relative_path TEXT NOT NULL,
    external_id TEXT,
    annotation_path TEXT,
    source_version TEXT NOT NULL DEFAULT ''
);
```

Add migration-safe `ALTER TABLE` statements and typed read/write helpers.

Replace `DataSourceAnalysis.warnings` with:

```rust
pub problems: Vec<InspectionProblem>,
pub unsupported_object_count: u32,
pub detection_confidence: u8,
pub annotation_path: Option<String>,
```

Keep a temporary compatibility accessor only if existing frontend tests require it
during the same task.

**Step 4: Run tests to verify they pass**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml source_mapping
cargo test --manifest-path src-tauri\Cargo.toml inspection_problem
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/project_fs.rs src-tauri/src/storage.rs src-tauri/src/datasets.rs src-tauri/src/domain.rs src/types/domain.ts
git commit -m "feat: persist annotation source mappings"
```

### Task 2: Adapter Registry And Detection

**Files:**
- Modify: `src-tauri/src/importers/mod.rs`
- Create: `src-tauri/src/importers/adapter.rs`
- Create: `src-tauri/src/importers/detect.rs`
- Modify: `src-tauri/src/datasets.rs`
- Test: `src-tauri/src/importers/detect.rs`
- Test: `src-tauri/src/datasets.rs`

**Step 1: Write failing format detection tests**

Create table-driven tests for:

```rust
[
    ("yolo-detect", fixture("yolo_detect")),
    ("yolo-seg", fixture("yolo_seg")),
    ("voc-detect", fixture("voc_sidecar")),
    ("coco", fixture("coco")),
    ("labelme", fixture("labelme")),
    ("image-directory", fixture("images_only")),
]
```

Also test that a JSON file with `images`, `annotations`, and `categories` is COCO,
while a JSON file with `imagePath` and `shapes` is LabelMe.

**Step 2: Run tests to verify they fail**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml format_detection
```

Expected: FAIL because no adapter registry or JSON structural detection exists.

**Step 3: Implement the adapter contract**

Define:

```rust
pub trait AnnotationFormatAdapter {
    fn format(&self) -> &'static str;
    fn detect(&self, selection: &SourceSelection) -> DetectionResult;
    fn inspect(&self, selection: &SourceSelection) -> Result<AdapterInspection, String>;
    fn load_image(
        &self,
        source: &DatasetSourceContext,
        image: &StoredImageSource,
        classes: &[StoredClass],
    ) -> Result<Vec<AnnotationObject>, String>;
    fn sync_image(&self, request: &SyncImageRequest) -> Result<SourceSyncResult, String>;
    fn sync_dataset(&self, request: &SyncDatasetRequest) -> Result<SourceSyncResult, String>;
    fn export(&self, request: &ExportRequest) -> Result<ExportSummary, String>;
}
```

Add a registry that evaluates every adapter, chooses the highest positive score,
and reports ambiguity when top scores are equal.

Move generic filesystem collection and tree-building helpers out of
`datasets.rs` into `importers/adapter.rs`.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml format_detection
cargo test --manifest-path src-tauri\Cargo.toml datasets::tests
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers src-tauri/src/datasets.rs
git commit -m "feat: add annotation format adapter registry"
```

### Task 3: Migrate YOLO Detect And Pascal VOC

**Files:**
- Modify: `src-tauri/src/importers/yolo.rs`
- Modify: `src-tauri/src/importers/voc.rs`
- Create: `src-tauri/src/importers/yolo_adapter.rs`
- Create: `src-tauri/src/importers/voc_adapter.rs`
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/datasets.rs`
- Test: `src-tauri/src/importers/yolo_adapter.rs`
- Test: `src-tauri/src/importers/voc_adapter.rs`

**Step 1: Write failing round-trip tests**

For YOLO Detect:

- Inspect nested `images/train` and `labels/train`.
- Load a BBox.
- Edit it.
- Synchronize it.
- Reparse the TXT and assert equivalent geometry.

For Pascal VOC:

- Test XML beside an image.
- Test `Annotations/a.xml` paired with `JPEGImages/a.jpg`.
- Preserve `difficult`, `truncated`, `pose`, and confidence.

**Step 2: Run tests to verify they fail**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml yolo_adapter
cargo test --manifest-path src-tauri\Cargo.toml voc_adapter
```

Expected: FAIL because current parsing is called directly from the repository and
VOC cannot resolve the common split-directory layout.

**Step 3: Implement adapters and atomic sidecar writes**

Move source path resolution into the adapters. Write sidecars to a temporary file
in the target directory and replace the destination only after serialization
succeeds.

Use source version:

```text
<file-size>:<modified-unix-nanos>
```

Reject synchronization when the current version differs from the imported version.

Update `SampleRepository::image_annotation_state` and
`save_image_annotations_with_revision` to call the selected adapter.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml yolo_adapter
cargo test --manifest-path src-tauri\Cargo.toml voc_adapter
cargo test --manifest-path src-tauri\Cargo.toml datasets::tests
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers src-tauri/src/domain.rs src-tauri/src/datasets.rs
git commit -m "refactor: route yolo and voc through adapters"
```

### Task 4: YOLO Segmentation Round Trip

**Files:**
- Modify: `src-tauri/src/importers/yolo.rs`
- Modify: `src-tauri/src/importers/yolo_adapter.rs`
- Modify: `src-tauri/src/domain.rs`
- Test: `src-tauri/src/importers/yolo.rs`
- Test: `src-tauri/src/importers/yolo_adapter.rs`
- Test: `src-tauri/tests/real_datasets.rs`

**Step 1: Write the failing writer tests**

Add:

```rust
let objects = vec![AnnotationObject::polygon(
    "ann-1".to_string(),
    2,
    "scratch".to_string(),
    vec![
        Point { x: 10.0, y: 20.0 },
        Point { x: 80.0, y: 20.0 },
        Point { x: 50.0, y: 90.0 },
    ],
)];
```

Assert output:

```text
2 0.100000 0.100000 0.800000 0.100000 0.500000 0.450000
```

for a 100x200 image. Add malformed mixed-label inspection tests with exact line
numbers.

**Step 2: Verify failure**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml yolo_polygon
```

Expected: FAIL because the writer currently skips polygons.

**Step 3: Implement polygon writing and detection**

Add `annotations_to_yolo_polygon_lines`. Detect dataset type by validating sampled
non-empty label lines. Treat mixed valid BBox and polygon records as an inspection
error unless the user explicitly chooses a lossy conversion.

Enable per-image source synchronization for `yolo-seg`.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml yolo_polygon
cargo test --manifest-path src-tauri\Cargo.toml real_datasets
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers/yolo.rs src-tauri/src/importers/yolo_adapter.rs src-tauri/src/domain.rs src-tauri/tests/real_datasets.rs
git commit -m "feat: support yolo segmentation round trips"
```

### Task 5: LabelMe Import, Writeback, And Export

**Files:**
- Create: `src-tauri/src/importers/labelme.rs`
- Modify: `src-tauri/src/importers/mod.rs`
- Modify: `src-tauri/src/importers/adapter.rs`
- Modify: `src-tauri/src/domain.rs`
- Test: `src-tauri/src/importers/labelme.rs`

**Step 1: Write failing LabelMe tests**

Use a fixture containing:

```json
{
  "version": "5.5.0",
  "flags": {"reviewed": true},
  "shapes": [
    {"label": "defect", "points": [[10, 20], [40, 60]], "group_id": 7, "shape_type": "rectangle", "flags": {}},
    {"label": "scratch", "points": [[5, 5], [30, 5], [20, 40]], "group_id": null, "shape_type": "polygon", "flags": {"hard": true}}
  ],
  "imagePath": "a.png",
  "imageData": null,
  "imageHeight": 100,
  "imageWidth": 100
}
```

Assert:

- Rectangle becomes BBox.
- Polygon remains Polygon.
- `group_id`, file flags, and shape flags survive write and reparse.
- Unsupported shapes such as `circle` are reported, not silently dropped.

**Step 2: Verify failure**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml labelme
```

Expected: FAIL because LabelMe support does not exist.

**Step 3: Implement the LabelMe adapter**

Deserialize unknown JSON fields with `#[serde(flatten)]` maps so they can be
retained in namespaced attributes. Support rectangle and polygon only.

For linked mode, atomically rewrite one sidecar JSON per image. For copied mode,
materialize embedded `imageData` only when no external image exists.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml labelme
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers/labelme.rs src-tauri/src/importers/mod.rs src-tauri/src/importers/adapter.rs src-tauri/src/domain.rs
git commit -m "feat: support labelme datasets"
```

### Task 6: COCO Import And Dataset-Level Synchronization

**Files:**
- Create: `src-tauri/src/importers/coco.rs`
- Modify: `src-tauri/src/importers/mod.rs`
- Modify: `src-tauri/src/importers/adapter.rs`
- Modify: `src-tauri/src/storage.rs`
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/http_backend.rs`
- Modify: `src/api/tauri.ts`
- Test: `src-tauri/src/importers/coco.rs`
- Test: `src-tauri/tests/real_datasets.rs`

**Step 1: Write failing COCO tests**

Create a fixture with:

- Two images.
- Two categories.
- One BBox annotation.
- One polygon segmentation annotation.
- `iscrowd`, `area`, and extra top-level info/licenses.

Assert import preserves external ids and supported metadata. Add blocking tests for
duplicate image ids, missing image files, and malformed polygon arrays.

Add a synchronization test:

1. Import linked COCO.
2. Edit one image.
3. Call `sync_dataset_source`.
4. Reparse JSON.
5. Assert the edited annotation changed and the unrelated image/info/licenses did
   not disappear.

**Step 2: Verify failure**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml coco
```

Expected: FAIL because COCO import and synchronization do not exist.

**Step 3: Implement COCO adapter and command**

Parse numeric external ids as strings in source mappings. Convert:

```text
[x, y, width, height] -> BBox
[x1, y1, ...] -> Polygon
```

Report RLE, keypoints, captions, and panoptic records as unsupported.

Add:

```rust
fn sync_dataset_source(project_id: String) -> Result<SourceSyncResult, String>
```

Expose it through both Tauri and the local HTTP backend. Synchronization rewrites
to a temporary JSON file, retains supported top-level metadata, validates the
source version, and atomically replaces the original file.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml coco
cargo test --manifest-path src-tauri\Cargo.toml real_datasets
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers/coco.rs src-tauri/src/importers/mod.rs src-tauri/src/importers/adapter.rs src-tauri/src/storage.rs src-tauri/src/domain.rs src-tauri/src/lib.rs src-tauri/src/http_backend.rs src/api/tauri.ts src-tauri/tests/real_datasets.rs
git commit -m "feat: support coco import and source sync"
```

### Task 7: Unified Import Confirmation UI

**Files:**
- Modify: `src/types/domain.ts`
- Modify: `src/api/tauri.ts`
- Modify: `src/App.tsx`
- Modify: `src/styles.css`
- Test: `src/App.test.tsx`
- Test: `src/styles.test.ts`

**Step 1: Write failing frontend tests**

Add tests that:

- Display `YOLO Segmentation`, `COCO`, and `LabelMe`.
- Render errors, warnings, and unsupported object counts separately.
- Disable the confirm button when any `error` exists.
- Allow a format override and re-run analysis with the override.
- Offer linked and copied modes when the adapter supports both.
- Show an explicit COCO “同步源标注” action in project pages.

**Step 2: Verify failure**

Run:

```powershell
npm test -- --run src/App.test.tsx src/styles.test.ts
```

Expected: FAIL because the current dialog only displays a warning string list and
VOC/YOLO BBox options.

**Step 3: Implement the UI**

Extend `DataSourceAnalysis` and `DatasetFormat`. Keep the confirmation panel dense:

- Summary metrics on the left.
- Source tree on the right.
- Problem list below the summary.
- Format override and source mode controls above the final confirm button.

Do not allow confirmation while analysis is stale or contains blocking errors.

**Step 4: Verify**

Run:

```powershell
npm test -- --run src/App.test.tsx src/styles.test.ts
npm run build
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src/types/domain.ts src/api/tauri.ts src/App.tsx src/styles.css src/App.test.tsx src/styles.test.ts
git commit -m "feat: expand dataset import confirmation"
```

### Task 8: Real Exporters And Loss Validation

**Files:**
- Create: `src-tauri/src/exporters/mod.rs`
- Create: `src-tauri/src/exporters/yolo.rs`
- Create: `src-tauri/src/exporters/voc.rs`
- Create: `src-tauri/src/exporters/coco.rs`
- Create: `src-tauri/src/exporters/labelme.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/storage.rs`
- Modify: `src/App.tsx`
- Modify: `src/types/domain.ts`
- Test: `src-tauri/src/exporters/mod.rs`
- Test: `src/App.test.tsx`

**Step 1: Write failing exporter tests**

For each format:

1. Import its fixture.
2. Create a snapshot.
3. Export.
4. Re-inspect exported files with the adapter.
5. Assert equivalent supported images, classes, geometry, and metadata.

Add a test that exporting polygons to YOLO Detect returns a blocking lossy-export
problem unless `polygonPolicy = "bbox"` or `"skip"` is explicitly provided.

**Step 2: Verify failure**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml exporters
```

Expected: FAIL because `export_dataset` writes placeholder files.

**Step 3: Implement exporters**

Change export request to:

```rust
pub struct ExportOptions {
    pub format: String,
    pub polygon_policy: Option<String>,
    pub include_images: bool,
}
```

Each exporter writes a machine-readable `export-manifest.json` containing counts,
problems, and options. Record those fields with the export record.

Update the UI to offer YOLO Detect, YOLO Seg, VOC, COCO, and LabelMe plus any
required loss policy.

**Step 4: Verify**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml exporters
npm test -- --run src/App.test.tsx
npm run build
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/exporters src-tauri/src/lib.rs src-tauri/src/domain.rs src-tauri/src/storage.rs src/App.tsx src/types/domain.ts src/App.test.tsx
git commit -m "feat: export supported annotation formats"
```

### Task 9: Fixture Matrix And Full Verification

**Files:**
- Create: `src-tauri/tests/fixtures/formats/yolo_detect/`
- Create: `src-tauri/tests/fixtures/formats/yolo_seg/`
- Create: `src-tauri/tests/fixtures/formats/voc_sidecar/`
- Create: `src-tauri/tests/fixtures/formats/voc_split/`
- Create: `src-tauri/tests/fixtures/formats/coco/`
- Create: `src-tauri/tests/fixtures/formats/labelme/`
- Create: `src-tauri/tests/format_matrix.rs`
- Modify: `docs/plans/2026-07-16-annotation-format-compatibility.md`

**Step 1: Add the matrix test**

Use one common assertion helper:

```rust
fn assert_round_trip(format: &str, fixture: &Path) {
    let inspection = inspect_fixture(format, fixture);
    assert!(inspection.problems.iter().all(|item| item.severity != "error"));
    let imported = import_fixture(format, fixture);
    let exported = export_fixture(&imported, format);
    let reimported = import_fixture(format, &exported);
    assert_equivalent_supported_content(&imported, &reimported);
}
```

Include separate malformed fixtures that prove:

- Missing images block COCO import.
- Invalid YOLO line numbers are reported.
- Invalid VOC XML is reported by file.
- Unsupported LabelMe shapes increment the unsupported count.
- External source changes cause synchronization conflicts.

**Step 2: Run the matrix**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo test --manifest-path src-tauri\Cargo.toml --test format_matrix
```

Expected: PASS for all supported round trips and expected problem cases.

**Step 3: Run all automated verification**

Run:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
cargo fmt --manifest-path src-tauri\Cargo.toml -- --check
cargo test --manifest-path src-tauri\Cargo.toml
npm test -- --run
npm run build
```

Expected:

- Rust formatting exits 0.
- All Rust tests pass.
- All frontend tests pass.
- Production frontend build exits 0.

**Step 4: Run desktop smoke verification**

Start:

```powershell
$env:CARGO_TARGET_DIR='L:\codex_build\image_annotation_tauri_target'
npm run tauri -- dev
```

Verify with one fixture for each format:

- Folder/file picker opens.
- Inspection shows the correct format and counts.
- Import creates a usable dataset.
- Existing BBox/Polygon objects render.
- Editing and saving updates linked sidecars where supported.
- COCO source synchronization updates its JSON.
- Each export target produces re-importable files.

**Step 5: Record evidence and commit**

Update this plan with final test counts and any explicit unsupported records.

```powershell
git add src-tauri/tests/fixtures/formats src-tauri/tests/format_matrix.rs docs/plans/2026-07-16-annotation-format-compatibility.md
git commit -m "test: verify annotation format compatibility matrix"
```
