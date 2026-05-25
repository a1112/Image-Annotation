# Image Preview Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a read-only image preview dialog to the dataset project image tab and connect its marker action to the independent annotation console.

**Architecture:** Keep image browsing state in `ProjectWorkspace`, pass a preview handler into `renderProjectTab`, and render a modal component beside the project surface. Reuse existing asset URL and annotation data. Use `open_annotation_window` for desktop annotation consoles, with browser navigation as a backend-unavailable fallback.

**Tech Stack:** React 18, TypeScript, Vitest, Testing Library, existing CSS in `src/styles.css`.

---

### Task 1: Preview Dialog Behavior

**Files:**
- Modify: `src/App.test.tsx`
- Modify: `src/App.tsx`
- Modify: `src/styles.css`

**Step 1: Write the failing test**

Add a test that opens a project, switches to `图片`, clicks `预览`, and expects a dialog named `图像预览` with the real image, filename, dimensions, object count, and annotation overlay.

**Step 2: Run test to verify it fails**

Run: `npm test -- --run src/App.test.tsx`

Expected: FAIL because no `预览` button or preview dialog exists.

**Step 3: Write minimal implementation**

Add `previewImageId` state to `ProjectWorkspace`, create an `ImagePreviewDialog` component, add a `预览` button to each image tile, and render the modal when a selected image exists.

**Step 4: Run test to verify it passes**

Run: `npm test -- --run src/App.test.tsx`

Expected: PASS.

### Task 2: Marker Routing

**Files:**
- Modify: `src/App.test.tsx`
- Modify: `src/App.tsx`

**Step 1: Write the failing test**

Add a test that opens the preview dialog, clicks `标记`, and expects the annotation workspace heading and selected image to appear.

**Step 2: Run test to verify it fails**

Run: `npm test -- --run src/App.test.tsx`

Expected: FAIL until `标记` is wired to navigation.

**Step 3: Write minimal implementation**

Wire the dialog `标记` action to `navigate("#/annotate/${projectId}/${image.id}")`.

**Step 4: Run test to verify it passes**

Run: `npm test -- --run src/App.test.tsx`

Expected: PASS.

### Task 3: Layout Polish

**Files:**
- Modify: `src/App.tsx`
- Modify: `src/styles.css`

**Step 1: Write or extend assertions**

Assert the image tab heading includes a short subtitle, keeping the title area separately queryable from paging controls.

**Step 2: Implement styles**

Add top-aligned header classes, preview modal layout, image stage, metadata grid, and compact tile action styles.

**Step 3: Verify**

Run: `npm test -- --run src/App.test.tsx`
Run: `npm run build`

Expected: both pass.

### Task 4: Independent Annotation Console

**Files:**
- Modify: `src/App.test.tsx`
- Modify: `src/App.tsx`
- Modify: `src/styles.css`
- Modify: `src-tauri/src/windows.rs`
- Modify: `src-tauri/tests/real_datasets.rs`

**Step 1: Write failing tests**

Add tests that require `#/annotate/{projectId}/{imageId}` to render without the main app banner or primary navigation, require the preview `标记` action to call `open_annotation_window`, and require existing Tauri annotation windows to jump to the requested image.

**Step 2: Implement**

Return `AnnotationWorkspace` before the main shell when the route is `annotate`. Add annotation-window controls to the workspace toolbar. Call `openAnnotationWindow(projectId, imageId)` from the preview marker action and fall back to hash navigation only when the backend is unavailable. In Tauri, when the project annotation window already exists, evaluate a hash-navigation script before focusing it.

**Step 3: Verify**

Run: `npm test -- --run src/App.test.tsx`
Run: `npm run build`
Run: `cargo test --manifest-path src-tauri/Cargo.toml`
