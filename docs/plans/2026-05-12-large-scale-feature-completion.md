# Image Annotation Large-Scale Feature Completion Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Turn the current UI prototype into a usable Tauri desktop image annotation application with persistent dataset management, grouped data views, annotation editing, quality checks, and export workflows.

**Architecture:** Keep React as the desktop UI and Tauri/Rust as the local application runtime. Rust owns filesystem access, SQLite metadata, import/export jobs, quality checks, and annotation persistence; React consumes typed Tauri commands and keeps UI state local to each workspace screen.

**Tech Stack:** React, TypeScript, Vite, Vitest, Testing Library, Tauri v2, Rust, serde, SQLite via `rusqlite` or `sqlx`, filesystem-backed image storage, COCO/YOLO/LabelMe JSON exporters.

---

### Task 1: Shared Domain Types

**Files:**
- Create: `src/types/domain.ts`
- Modify: `src/App.tsx`
- Modify: `src-tauri/src/domain.rs`
- Test: `src/App.test.tsx`
- Test: `src-tauri/src/domain.rs`

**Step 1: Write the failing tests**

Add frontend and Rust tests that assert `DatasetProject`, `TagGroup`, `ImageRecord`, `AnnotationObject`, `ClassSchema`, `TaskSummary`, `QualityIssue`, and `ExportJob` exist with stable ids and serializable fields.

**Step 2: Run tests to verify failure**

Run:
```bash
npm test -- --run
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: TypeScript and Rust fail because the full domain model does not exist.

**Step 3: Implement minimal shared models**

Create TypeScript interfaces and Rust structs using the same camelCase JSON field names. Keep sample data in Rust until SQLite is introduced.

**Step 4: Verify**

Run:
```bash
npm test -- --run
cargo test --manifest-path src-tauri/Cargo.toml
```

### Task 2: Local Storage Layer

**Files:**
- Create: `src-tauri/src/storage.rs`
- Create: `src-tauri/src/schema.sql`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/storage.rs`

**Step 1: Write the failing tests**

Test that a new workspace database can initialize tables for projects, images, tags, annotations, classes, tasks, quality issues, and export jobs.

**Step 2: Run test to verify it fails**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml storage
```

Expected: FAIL because storage module and schema do not exist.

**Step 3: Implement minimal SQLite storage**

Use `rusqlite` first for local embedded storage. Add migration bootstrap and repository helpers for creating/listing projects.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml storage
```

### Task 3: Import Images Workflow

**Files:**
- Create: `src-tauri/src/importer.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Test: `src-tauri/src/importer.rs`
- Test: `src/App.test.tsx`

**Step 1: Write failing tests**

Test that importing a directory records supported image extensions, skips duplicates by path/hash, assigns initial tags, and returns an import summary.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml importer
```

Expected: FAIL because importer does not exist.

**Step 3: Implement importer**

Add `start_import_job`, `get_import_job`, and `cancel_import_job` commands. Keep jobs synchronous for the first pass, then move to worker runtime.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml importer
npm test -- --run
```

### Task 4: Data Groups Builder

**Files:**
- Create: `src-tauri/src/groups.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Test: `src-tauri/src/groups.rs`
- Test: `src/App.test.tsx`

**Step 1: Write failing tests**

Test that a saved group query can combine dimensions such as `split=train`, `scene=urban`, `status=approved`, and returns matching image counts.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml groups
```

Expected: FAIL because group query engine does not exist.

**Step 3: Implement group query model**

Store group filters as structured JSON plus indexed tag rows. Expose `create_tag_group`, `update_tag_group`, `delete_tag_group`, and `preview_tag_group`.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml groups
npm test -- --run
```

### Task 5: Annotation Editor Core

**Files:**
- Create: `src/annotation/geometry.ts`
- Create: `src/annotation/AnnotationCanvas.tsx`
- Modify: `src/App.tsx`
- Test: `src/annotation/geometry.test.ts`
- Test: `src/App.test.tsx`

**Step 1: Write failing tests**

Test bbox creation, normalization, selection, resize handles, clamping to image bounds, and coordinate serialization.

**Step 2: Run test**

Run:
```bash
npm test -- --run src/annotation/geometry.test.ts
```

Expected: FAIL because geometry module does not exist.

**Step 3: Implement bbox editor foundation**

Use SVG or canvas overlay with React-managed objects for the first pass. Prioritize BBox editing before polygon, mask, and keypoints.

**Step 4: Verify**

Run:
```bash
npm test -- --run
```

### Task 6: Annotation Persistence

**Files:**
- Create: `src-tauri/src/annotations.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/annotations.rs`

**Step 1: Write failing tests**

Test saving and loading annotations for one image, including object id, class id, bbox coordinates, attributes, and updated timestamp.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml annotations
```

Expected: FAIL because annotation repository does not exist.

**Step 3: Implement commands**

Add `get_image_annotations`, `save_image_annotations`, and `mark_image_status`.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml annotations
```

### Task 7: Quality Checks

**Files:**
- Create: `src-tauri/src/quality.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Test: `src-tauri/src/quality.rs`

**Step 1: Write failing tests**

Test overlap warnings, small boxes, missing class labels, duplicate images, and unlabeled image detection.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml quality
```

Expected: FAIL because quality module does not exist.

**Step 3: Implement quality check service**

Start with deterministic checks that run on demand per project/group. Store results in SQLite and surface counts in the UI.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml quality
npm test -- --run
```

### Task 8: Exporters

**Files:**
- Create: `src-tauri/src/exporters/mod.rs`
- Create: `src-tauri/src/exporters/coco.rs`
- Create: `src-tauri/src/exporters/yolo.rs`
- Create: `src-tauri/src/exporters/labelme.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/exporters/coco.rs`

**Step 1: Write failing tests**

Test that a small dataset exports valid COCO JSON with images, annotations, categories, and bbox coordinates.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml exporters
```

Expected: FAIL because exporter modules do not exist.

**Step 3: Implement COCO first**

Add `export_dataset_group` command. Implement COCO fully before YOLO and LabelMe.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml exporters
```

### Task 9: Task and Review Workflow

**Files:**
- Create: `src-tauri/src/tasks.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Test: `src-tauri/src/tasks.rs`

**Step 1: Write failing tests**

Test creating annotation tasks, assigning image groups, advancing image status, and moving approved images into export-ready state.

**Step 2: Run test**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml tasks
```

Expected: FAIL because task service does not exist.

**Step 3: Implement task service**

Add local-only task assignment first. Do not add users/permissions until local workflow works.

**Step 4: Verify**

Run:
```bash
cargo test --manifest-path src-tauri/Cargo.toml tasks
npm test -- --run
```

### Task 10: Full App Verification

**Files:**
- Modify: `docs/plans/2026-05-12-large-scale-feature-completion.md`

**Step 1: Run all tests**

Run:
```bash
npm test -- --run
cargo test --manifest-path src-tauri/Cargo.toml
```

**Step 2: Run builds**

Run:
```bash
npm run build
npm run tauri -- build
```

**Step 3: Manual smoke test**

Open `http://localhost:1440`, verify Chinese UI, dataset cards, project tabs, annotation workspace, and export page.
