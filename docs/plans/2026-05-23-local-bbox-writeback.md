# Local BBox Writeback Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the first-phase labelImg-like workflow for local BBox annotation with in-place Pascal VOC XML and YOLO detection TXT writeback.

**Architecture:** Reuse the existing React annotation workspace and Rust `SampleRepository`. Local-linked projects continue to store a lightweight project manifest and SQLite index while resolving image files from the original source directory; save persists internal JSON/SQLite state and writes the source format sidecar.

**Tech Stack:** Tauri 2, Rust, SQLite via `rusqlite`, React 18, TypeScript, Vitest, Testing Library.

---

### Task 1: YOLO BBox Writer

**Files:**
- Modify: `src-tauri/src/importers/yolo.rs`
- Test: `src-tauri/tests/real_datasets.rs`

**Step 1: Write the failing test**

Add this import:

```rust
use image_annotation_lib::domain::{AnnotationObject, BBox};
```

Extend the existing YOLO import line to include the new writer:

```rust
use image_annotation_lib::importers::yolo::{
    annotations_to_yolo_lines, parse_yolo_bbox_line, parse_yolo_polygon_line,
};
```

Add this test:

```rust
#[test]
fn yolo_bbox_objects_write_normalized_detection_lines() {
    let objects = vec![AnnotationObject::bbox(
        "ann-1".to_string(),
        2,
        "car".to_string(),
        BBox {
            x: 256.0,
            y: 96.0,
            width: 128.0,
            height: 48.0,
        },
    )];

    let lines = annotations_to_yolo_lines(&objects, 640, 480).unwrap();

    assert_eq!(lines, "2 0.500000 0.250000 0.200000 0.100000\n");
}
```

**Step 2: Run test to verify it fails**

Run:

```powershell
cd src-tauri
cargo test yolo_bbox_objects_write_normalized_detection_lines
```

Expected: fail with unresolved import/function `annotations_to_yolo_lines`.

**Step 3: Write minimal implementation**

Add to `src-tauri/src/importers/yolo.rs`:

```rust
pub fn annotations_to_yolo_lines(
    objects: &[AnnotationObject],
    image_width: u32,
    image_height: u32,
) -> Result<String, String> {
    if image_width == 0 || image_height == 0 {
        return Err("image dimensions are required for YOLO export".to_string());
    }

    let mut lines = String::new();
    for object in objects {
        let Some(bbox) = object.bbox.as_ref() else {
            continue;
        };
        let width = bbox.width.max(1.0).min(image_width as f64);
        let height = bbox.height.max(1.0).min(image_height as f64);
        let center_x = (bbox.x + width / 2.0).clamp(0.0, image_width as f64);
        let center_y = (bbox.y + height / 2.0).clamp(0.0, image_height as f64);
        lines.push_str(&format!(
            "{} {:.6} {:.6} {:.6} {:.6}\n",
            object.class_id,
            center_x / image_width as f64,
            center_y / image_height as f64,
            width / image_width as f64,
            height / image_height as f64,
        ));
    }
    Ok(lines)
}
```

**Step 4: Run test to verify it passes**

Run:

```powershell
cd src-tauri
cargo test yolo_bbox_objects_write_normalized_detection_lines
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/importers/yolo.rs src-tauri/tests/real_datasets.rs
git commit -m "feat: write yolo bbox annotations"
```

### Task 2: Local YOLO Label Discovery

**Files:**
- Modify: `src-tauri/src/datasets.rs`
- Test: `src-tauri/src/datasets.rs`

**Step 1: Write the failing test**

Add a test beside `opens_local_pascal_voc_folder_without_copying_images`:

```rust
#[test]
fn opens_local_yolo_folder_with_existing_classes_and_labels() {
    let source_root = std::env::temp_dir().join("image_annotation_yolo_open_test");
    let _ = fs::remove_dir_all(&source_root);
    fs::create_dir_all(source_root.join("images").join("train")).unwrap();
    fs::create_dir_all(source_root.join("labels").join("train")).unwrap();
    let image_path = source_root.join("images").join("train").join("sample.png");
    write_demo_image(&image_path, 1).unwrap();
    fs::write(source_root.join("classes.txt"), "defect\nscratch\n").unwrap();
    fs::write(
        source_root.join("labels").join("train").join("sample.txt"),
        "1 0.500000 0.500000 0.250000 0.200000\n",
    )
    .unwrap();

    let project = open_local_dataset(&source_root.to_string_lossy(), "yolo-detect").unwrap();
    let paths = project_fs::project_paths(&project.id);
    let repository = domain::SampleRepository::new();
    let state = repository.image_annotation_state(&project.id, "images_train_sample");

    assert_eq!(project.class_count, 2);
    assert_eq!(state.objects[0].label, "scratch");
    assert_eq!(state.objects[0].class_id, 1);

    let _ = fs::remove_dir_all(paths.root);
    let _ = fs::remove_dir_all(source_root);
}
```

**Step 2: Run test to verify it fails**

Run:

```powershell
cd src-tauri
cargo test opens_local_yolo_folder_with_existing_classes_and_labels
```

Expected: fail because local YOLO labels use fallback classes or cannot find the label sidecar under a local-linked root.

**Step 3: Write minimal implementation**

In `open_local_dataset`, replace the label selection block with:

```rust
let labels = match dataset_type {
    "voc-detect" => indexed_voc_labels(&canonical),
    "yolo-detect" => indexed_yolo_labels(&canonical),
    _ => Vec::new(),
};
```

Add helpers near `indexed_voc_labels`:

```rust
fn indexed_yolo_labels(root: &Path) -> Vec<String> {
    for candidate in ["classes.txt", "obj.names"] {
        let path = root.join(candidate);
        if let Ok(data) = fs::read_to_string(path) {
            let labels = label_lines(&data);
            if !labels.is_empty() {
                return labels;
            }
        }
    }

    let yaml_path = root.join("data.yaml");
    if let Ok(data) = fs::read_to_string(yaml_path) {
        let labels = parse_simple_yaml_names(&data);
        if !labels.is_empty() {
            return labels;
        }
    }

    let max_class_id = indexed_yolo_label_ids(root).into_iter().max();
    max_class_id
        .map(|max_id| (0..=max_id).map(|id| format!("class_{id}")).collect())
        .unwrap_or_default()
}

fn label_lines(data: &str) -> Vec<String> {
    data.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_simple_yaml_names(data: &str) -> Vec<String> {
    let Some((_, names)) = data.lines().find_map(|line| line.split_once("names:")) else {
        return Vec::new();
    };
    names
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|name| name.trim().trim_matches('"').trim_matches('\''))
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn indexed_yolo_label_ids(root: &Path) -> Vec<u32> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .map(|extension| extension.to_string_lossy().eq_ignore_ascii_case("txt"))
                    .unwrap_or(false)
        })
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|data| {
            data.lines()
                .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}
```

**Step 4: Run test to verify it passes**

Run:

```powershell
cd src-tauri
cargo test opens_local_yolo_folder_with_existing_classes_and_labels
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/datasets.rs
git commit -m "feat: index local yolo classes"
```

### Task 3: Local YOLO Sidecar Resolution And Writeback

**Files:**
- Modify: `src-tauri/src/domain.rs`
- Test: `src-tauri/src/datasets.rs`

**Step 1: Write the failing test**

Add this test in `src-tauri/src/datasets.rs`:

```rust
#[test]
fn saves_local_yolo_annotations_back_to_source_labels_tree() {
    let source_root = std::env::temp_dir().join("image_annotation_yolo_save_test");
    let _ = fs::remove_dir_all(&source_root);
    fs::create_dir_all(source_root.join("images").join("train")).unwrap();
    fs::create_dir_all(source_root.join("labels").join("train")).unwrap();
    let image_path = source_root.join("images").join("train").join("sample.png");
    write_demo_image(&image_path, 1).unwrap();
    fs::write(source_root.join("classes.txt"), "defect\nscratch\n").unwrap();

    let project = open_local_dataset(&source_root.to_string_lossy(), "yolo-detect").unwrap();
    let paths = project_fs::project_paths(&project.id);
    let repository = domain::SampleRepository::new();
    repository
        .save_image_annotations_with_revision(
            &project.id,
            "images_train_sample",
            None,
            vec![domain::AnnotationObject::bbox(
                "ann-1".to_string(),
                1,
                "scratch".to_string(),
                domain::BBox {
                    x: 160.0,
                    y: 84.0,
                    width: 320.0,
                    height: 210.0,
                },
            )],
        )
        .unwrap();

    let txt = fs::read_to_string(source_root.join("labels").join("train").join("sample.txt"))
        .unwrap();
    assert_eq!(txt, "1 0.500000 0.450000 0.500000 0.500000\n");

    let _ = fs::remove_dir_all(paths.root);
    let _ = fs::remove_dir_all(source_root);
}
```

**Step 2: Run test to verify it fails**

Run:

```powershell
cd src-tauri
cargo test saves_local_yolo_annotations_back_to_source_labels_tree
```

Expected: fail because `save_image_annotations_with_revision` currently writes VOC XML only for VOC projects and does not write YOLO TXT.

**Step 3: Write minimal implementation**

In `src-tauri/src/domain.rs`, add `yolo` writer calls in `save_image_annotations_with_revision` after the VOC branch:

```rust
if is_yolo_detect_project(project_id) {
    if let Some(image_path) = self.image_path(project_id, image_id) {
        let (width, height) = image::image_dimensions(&image_path).unwrap_or((0, 0));
        let label_path = yolo_label_path_for_image(project_id, &image_path);
        if let Some(parent) = label_path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let label_data = yolo::annotations_to_yolo_lines(&state.objects, width, height)?;
        fs::write(label_path, label_data).map_err(|err| err.to_string())?;
    }
}
```

Add helpers near `is_voc_project`:

```rust
fn is_yolo_detect_project(project_id: &str) -> bool {
    project_manifest(project_id)
        .map(|manifest| manifest.format == "yolo-detect")
        .unwrap_or(false)
}

fn yolo_label_path_for_image(project_id: &str, image_path: &Path) -> PathBuf {
    let manifest_root = project_manifest(project_id)
        .map(|manifest| PathBuf::from(manifest.root_path))
        .unwrap_or_else(|| project_fs::project_paths(project_id).raw);

    label_path_for_image(&manifest_root, image_path).unwrap_or_else(|| image_path.with_extension("txt"))
}
```

Update the existing YOLO read path in `image_annotation_state` to call `yolo_label_path_for_image(project_id, &image_path)` for local-linked projects and to read labels from `storage::read_classes`, not `coco_labels`.

**Step 4: Run test to verify it passes**

Run:

```powershell
cd src-tauri
cargo test saves_local_yolo_annotations_back_to_source_labels_tree
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src-tauri/src/domain.rs src-tauri/src/datasets.rs
git commit -m "feat: write local yolo labels in place"
```

### Task 4: Frontend Local Format Copy And Save Feedback

**Files:**
- Modify: `src/App.tsx`
- Test: `src/App.test.tsx`

**Step 1: Write the failing tests**

Add a test for the data submit copy:

```tsx
it("数据提交弹窗说明本机目录会原地写回 VOC 或 YOLO", async () => {
  const user = userEvent.setup();
  render(<App />);

  await user.click(screen.getByRole("button", { name: "数据提交" }));

  expect(screen.getByText(/保存时原地写回 XML 或 TXT/)).toBeInTheDocument();
  expect(screen.getByRole("option", { name: "YOLO BBox TXT" })).toBeInTheDocument();
});
```

Add a save feedback test near the existing save tests:

```tsx
it("标注控制台保存后显示原地写回提示", async () => {
  const user = userEvent.setup();
  window.location.hash = "#/annotate/coco128/000000000009";

  render(<App />);

  await user.click(await screen.findByRole("button", { name: "保存标注" }));

  expect(await screen.findByText(/已保存并写回标注文件/)).toBeInTheDocument();
});
```

**Step 2: Run tests to verify they fail**

Run:

```powershell
npm test -- src/App.test.tsx --run
```

Expected: fail because the copy/options and save message are not updated.

**Step 3: Write minimal implementation**

In `DataSubmitDialog`, update the local option text and option labels:

```tsx
<p>不复制图片，直接索引本机 VOC / YOLO BBox 目录，保存时原地写回 XML 或 TXT。</p>
```

```tsx
<option value="voc-detect">Pascal VOC BBox XML</option>
<option value="yolo-detect">YOLO BBox TXT</option>
```

In `AnnotationWorkspace.save`, replace:

```tsx
setSaveMessage(`已保存 ${result.savedAt}`);
```

with:

```tsx
setSaveMessage(`已保存并写回标注文件 ${result.savedAt}`);
```

**Step 4: Run tests to verify they pass**

Run:

```powershell
npm test -- src/App.test.tsx --run
```

Expected: PASS.

**Step 5: Commit**

```powershell
git add src/App.tsx src/App.test.tsx
git commit -m "feat: clarify local bbox writeback"
```

### Task 5: Full Verification

**Files:**
- No code changes unless verification finds a bug.

**Step 1: Run backend tests**

Run:

```powershell
cd src-tauri
cargo test
```

Expected: all Rust tests pass.

**Step 2: Run frontend tests**

Run:

```powershell
npm test -- --run
```

Expected: all Vitest tests pass.

**Step 3: Run build**

Run:

```powershell
npm run build
```

Expected: TypeScript and Vite build complete without errors.

**Step 4: Commit verification-only fixes if needed**

Only if fixes were required:

```powershell
git add <changed-files>
git commit -m "fix: stabilize local bbox writeback"
```

**Step 5: Report result**

Summarize:

- VOC in-place read/write status.
- YOLO in-place read/write status.
- Tests run and their outcomes.
- Any remaining limitations, especially polygon/segmentation non-goals.
