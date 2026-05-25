# Class Sample Management Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a category sample view that lets users select a class and inspect matching project images.

**Architecture:** The backend adds a dedicated `list_class_samples` query that filters project images by annotation objects. The frontend adds a typed API wrapper and renders the results inside the existing project `类别` tab using the current thumbnail, preview, and annotation-window flows.

**Tech Stack:** Rust/Tauri commands, SQLite-backed repository helpers, React 18, TypeScript, Vitest, Testing Library.

---

### Task 1: Backend Class Sample Query

**Files:**
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/http_backend.rs`
- Test: `src-tauri/src/domain.rs`

**Step 1: Write the failing Rust test**

Add a test in `src-tauri/src/domain.rs` under `#[cfg(test)] mod tests`:

```rust
#[test]
fn lists_images_that_contain_selected_class_with_match_counts() {
    let repository = SampleRepository::new();
    let project_id = "class-sample-unit";
    let paths = project_fs::project_paths(project_id);
    let _ = std::fs::remove_dir_all(&paths.root);
    project_fs::ensure_workspace_project_dirs(project_id).unwrap();
    storage::initialize_project_database(&paths.sqlite).unwrap();

    let manifest = project_fs::ProjectManifest {
        id: project_id.to_string(),
        name: "Class Sample Unit".to_string(),
        source_dataset_key: "local-demo".to_string(),
        format: "yolo-detect".to_string(),
        root_path: paths.root.to_string_lossy().to_string(),
        created_at: now_unix_string(),
        class_count: 2,
        image_count: 2,
    };
    let images = vec![
        storage::StoredImage {
            id: "image-a".to_string(),
            file_name: "image-a.png".to_string(),
            width: 640,
            height: 480,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        },
        storage::StoredImage {
            id: "image-b".to_string(),
            file_name: "image-b.png".to_string(),
            width: 640,
            height: 480,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        },
    ];
    let classes = vec![
        storage::StoredClass { id: 0, label: "person".to_string(), color: "#1fa7ff".to_string() },
        storage::StoredClass { id: 1, label: "car".to_string(), color: "#cc54d8".to_string() },
    ];
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes).unwrap();

    repository
        .save_image_annotations_with_revision(
            project_id,
            "image-a",
            None,
            vec![
                AnnotationObject::bbox("ann-1".to_string(), 0, "person".to_string(), BBox { x: 1.0, y: 1.0, width: 10.0, height: 10.0 }),
                AnnotationObject::bbox("ann-2".to_string(), 0, "person".to_string(), BBox { x: 2.0, y: 2.0, width: 10.0, height: 10.0 }),
            ],
        )
        .unwrap();
    repository
        .save_image_annotations_with_revision(
            project_id,
            "image-b",
            None,
            vec![AnnotationObject::bbox("ann-3".to_string(), 1, "car".to_string(), BBox { x: 1.0, y: 1.0, width: 10.0, height: 10.0 })],
        )
        .unwrap();

    let samples = repository.class_samples(project_id, Some(0), "person", Some(0), Some(48));

    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].image.id, "image-a");
    assert_eq!(samples[0].match_count, 2);

    let _ = std::fs::remove_dir_all(paths.root);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test lists_images_that_contain_selected_class_with_match_counts --manifest-path src-tauri/Cargo.toml`

Expected: FAIL because `ClassSample` and `SampleRepository::class_samples` do not exist.

**Step 3: Write minimal backend implementation**

In `src-tauri/src/domain.rs`:

- Add a serializable `ClassSample` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassSample {
    pub image: DatasetImage,
    pub match_count: u32,
}
```

- Add `SampleRepository::class_samples(project_id, class_id, label, offset, limit)`.
- Use `self.project_images(project_id, None)` to get candidate images.
- For each image, call `self.image_annotation_state(project_id, &image.id)`.
- Count objects where `Some(object.class_id) == class_id` or `object.label == label`.
- Apply offset and limit to matched samples.

In `src-tauri/src/lib.rs`:

- Import `ClassSample`.
- Add a `#[tauri::command] fn list_class_samples(...) -> Result<Vec<ClassSample>, String>`.
- Register it in `tauri::generate_handler!`.

In `src-tauri/src/http_backend.rs`:

- Add an invoke branch for `"list_class_samples"`.
- Parse `projectId`, `classId`, `label`, `offset`, `limit`.

**Step 4: Run test to verify it passes**

Run: `cargo test lists_images_that_contain_selected_class_with_match_counts --manifest-path src-tauri/Cargo.toml`

Expected: PASS.

**Step 5: Run backend regression tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`

Expected: PASS.

**Step 6: Commit**

```bash
git add src-tauri/src/domain.rs src-tauri/src/lib.rs src-tauri/src/http_backend.rs
git commit -m "feat: add class sample backend query"
```

### Task 2: Frontend API and Type Contract

**Files:**
- Modify: `src/types/domain.ts`
- Modify: `src/api/tauri.ts`
- Test: `src/api/tauri.test.ts`

**Step 1: Write the failing API test**

Add a test in `src/api/tauri.test.ts` that imports `listClassSamples` and verifies:

```ts
await listClassSamples("coco128", { classId: 0, label: "person", offset: 0, limit: 48 });

expect(invoke).toHaveBeenCalledWith("list_class_samples", {
  projectId: "coco128",
  classId: 0,
  label: "person",
  offset: 0,
  limit: 48,
});
```

**Step 2: Run test to verify it fails**

Run: `npm test -- src/api/tauri.test.ts`

Expected: FAIL because `listClassSamples` does not exist.

**Step 3: Write minimal implementation**

In `src/types/domain.ts`, add:

```ts
export type ClassSample = {
  image: DatasetImage;
  matchCount: number;
};
```

In `src/api/tauri.ts`, import `ClassSample` and add:

```ts
export async function listClassSamples(
  projectId: string,
  query: { classId?: number; label: string; offset?: number; limit?: number },
): Promise<ClassSample[]> {
  return invokeRequired("list_class_samples", {
    projectId,
    classId: query.classId ?? null,
    label: query.label,
    offset: query.offset ?? null,
    limit: query.limit ?? null,
  });
}
```

**Step 4: Run test to verify it passes**

Run: `npm test -- src/api/tauri.test.ts`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/types/domain.ts src/api/tauri.ts src/api/tauri.test.ts
git commit -m "feat: add class sample frontend api"
```

### Task 3: Category Tab Sample UI

**Files:**
- Modify: `src/App.tsx`
- Modify: `src/App.test.tsx`
- Modify: `src/styles.css`

**Step 1: Write the failing UI test**

Add a test in `src/App.test.tsx`:

```ts
it("类别页可以按类别查看样本并打开预览", async () => {
  const user = userEvent.setup();
  render(<App />);

  await user.click(await screen.findByRole("button", { name: "打开" }));
  await user.click(screen.getByRole("button", { name: "类别" }));
  await user.click(screen.getByRole("button", { name: "查看 person 样本" }));

  await waitFor(() =>
    expect(invoke).toHaveBeenCalledWith("list_class_samples", {
      projectId: "coco128",
      classId: 0,
      label: "person",
      offset: 0,
      limit: 48,
    }),
  );
  expect(await screen.findByRole("heading", { name: "person 样本" })).toBeInTheDocument();
  expect(screen.getByText("2 个匹配对象")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "预览 000000000009.jpg" }));
  expect(screen.getByRole("dialog", { name: "图像预览" })).toBeInTheDocument();
});
```

Extend the mocked `get_project_detail` class entry to include `id: 0` after the type is updated, and mock `"list_class_samples"`.

**Step 2: Run test to verify it fails**

Run: `npm test -- src/App.test.tsx`

Expected: FAIL because the class tab has no sample action.

**Step 3: Write minimal UI implementation**

In `src/App.tsx`:

- Import `listClassSamples` and `ClassSample`.
- Add `id?: number` to class display handling after updating the domain type.
- Add state to `ProjectWorkspace`:

```ts
const [selectedClass, setSelectedClass] = useState<ClassStat | null>(null);
const [classSamples, setClassSamples] = useState<ClassSample[]>([]);
const [classSamplePage, setClassSamplePage] = useState(0);
const [classSampleError, setClassSampleError] = useState<string | null>(null);
```

- Load samples in an effect when `selectedClass` or `classSamplePage` changes.
- Derive `classSampleImages = classSamples.map((sample) => sample.image)`.
- Reuse `useImageAssetUrls` and `useImageAnnotations` for sample images.
- Pass class sample state and callbacks into `renderProjectTab`.
- In the `类别` case, render class rows with `查看 {label} 样本`.
- Render a sample grid below the class list when a class is selected.
- Use existing preview dialog by setting `previewImageId`.
- Add a `标记` button that calls the existing `openAnnotationConsole`.

In `src/styles.css`, add small layout styles for the selected class sample panel and sample metadata. Keep the styling consistent with existing `.image-grid`, `.image-tile`, and `.data-table`.

**Step 4: Run test to verify it passes**

Run: `npm test -- src/App.test.tsx`

Expected: PASS.

**Step 5: Run frontend regression tests**

Run: `npm test -- --run`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/App.tsx src/App.test.tsx src/styles.css
git commit -m "feat: show samples by class"
```

### Task 4: Final Verification

**Files:**
- Verify only.

**Step 1: Run full frontend build**

Run: `npm run build`

Expected: TypeScript and Vite build pass.

**Step 2: Run backend tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`

Expected: PASS.

**Step 3: Inspect git status**

Run: `git status --short`

Expected: clean working tree.
