# labelImg C++ Annotation Parity Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete the C++ labelImg annotation workflows inside the existing Image Annotation desktop application, including optional local AI.

**Architecture:** Add a versioned, lossless annotation geometry model and a testable editing command layer. Keep React canvas state/UI in the current workspace and Rust as the storage/format/AI boundary. Validate format compatibility before source label writes and never silently omit shapes.

**Tech Stack:** React 18, TypeScript, Canvas 2D, Vitest, Tauri 2/Rust, local Python bridges.

---

Design: `docs/plans/2026-09-27-labelimg-parity-design.md`. Preserve unrelated working-tree changes already present when this task began.

## Task 1: Shape schema and safe persistence

**Files:** `src/types/domain.ts`, `src-tauri/src/domain.rs`, `src-tauri/src/importers/{voc,yolo}.rs`, `src-tauri/src/storage.rs`, new shape fixtures/tests in Rust and TypeScript.

1. Write failing serde/round-trip tests for legacy bbox/polygon and new types: oriented rectangle, circle, line, linestrip, point, points, mask. Include metadata (class, difficult, group, description, flags, colors, AI score) and malformed geometry.
2. Run focused Rust tests; confirm intended failures.
3. Extend `AnnotationObject` as a backward-compatible discriminated type. Reuse `attributes` for existing metadata where appropriate; add typed geometry and mask fields. Reject non-finite/out-of-image coordinates and invalid point counts at save boundaries.
4. Preflight source-format conversion before committing a revision. Replace silent `filter_map`/`continue` omissions with explicit compatibility results. Keep native annotation readable when source write-back is unavailable, and report this distinction in `AnnotationSaveResult`.
5. Run focused Rust tests, `cargo test` from `src-tauri`, and frontend build. Commit only task files.

## Task 2: Editing command core

**Files:** new `src/annotation/editor.ts`, `src/annotation/geometry.ts`, corresponding `*.test.ts`, `src/App.tsx`.

1. Write failing tests for create/finish/cancel, polygon point add/remove, move/resize/rotate within image bounds, multiselect, duplicate/delete-all, one history entry per drag, undo/redo and redo invalidation.
2. Run focused Vitest tests; confirm intended failures.
3. Implement immutable commands such as `applyEdit(state, command)` returning next state and a history checkpoint. Keep transient hover/drag state outside persisted objects.
4. Wire existing bbox/polygon canvas handlers and toolbar actions to the command core. Preserve legacy saved object IDs and current shortcuts.
5. Run focused tests and `npm run build`; commit task files.

## Task 3: All shape drawing and inspection

**Files:** `src/App.tsx`, `src/styles.css`, `src/annotation/geometry.ts`, `src/App.test.tsx`.

1. Add interaction tests for each reference shape type and selection from both canvas and object list.
2. Add canvas rendering, hit testing, handles and bounds for oriented rectangles, circles, lines, linestrips, point(s), masks. Add insertion/removal of polygon and linestrip vertices.
3. Add object properties for class, difficult, group, description, flags, color, visibility, multi-select, filtering, sorting, and consistent label assignment when a draft completes.
4. Add zoom fit-width, brightness, visibility and focus-selected controls; keep pointer coordinates in original image space.
5. Run focused/full frontend tests and build; manually exercise a small real image; commit task files.

## Task 4: Image queue, save and verification

**Files:** `src/App.tsx`, `src/api/tauri.ts`, `src-tauri/src/{domain,lib,http_backend,storage}.rs`, relevant tests.

1. Write regression tests for image 121+, filmstrip following the current item, direct image links, unsaved changes on every image-switch path, auto-save, previous-box copying, and persistent verification.
2. Implement paginated queue loading/search and navigation. Route toolbar, thumbnail, keyboard and direct-route changes through one dirty-state/auto-save transition.
3. Persist verified status through Rust and return it in `AnnotationState`; ensure save/submit do not accidentally reset verification. Surface source-write status and errors.
4. Test API and Rust storage flows, then manually navigate a real dataset; commit task files.

## Task 5: Format compatibility and external annotations

**Files:** `src-tauri/src/importers/{voc,yolo}.rs`, new `src-tauri/src/importers/{labelme,createml}.rs`, `src-tauri/src/domain.rs`, `src/api/tauri.ts`, `src/App.tsx`, tests/fixtures.

1. Write golden import/export round-trip tests using reference fixtures (including Chinese paths/labels, class order, difficult, empty labels, invalid formats).
2. Implement LabelMe JSON lossless import/export and CreateML import/export; expose external annotation loading and save-as/save-directory controls.
3. Preflight every export/snapshot. For unsupported geometry, show exact object IDs/types and block lossy exports; native revisions remain intact.
4. Test all formats and real YOLO/VOC source write-back; commit task files.

## Task 6: Local AI process boundary

**Files:** new `src-tauri/src/ai.rs`, `src-tauri/src/{lib,http_backend,domain}.rs`, bundled `scripts/ai/*` with reference license, Rust tests, `src/api/tauri.ts`.

1. Write failing tests with fake Python executables for dependency detection, process progress, timeout, cancellation, stale-image requests, malformed JSON, invalid geometry, wrong classes and duplicate suppression.
2. Vendor/adapt the reference MIT ONNX and OSAM bridges with license notice; run as local child processes with bounded input, output and lifetime. Do not commit model weights.
3. Validate response shapes before returning and resolve image paths only through project data. Never mutate revisions inside the AI command.
4. Expose dependency/model status, load/run/cancel commands through Tauri and HTTP fallback. Test both transport paths and process cleanup; commit task files.

## Task 7: AI annotation UI

**Files:** `src/App.tsx`, `src/styles.css`, `src/App.test.tsx`, `src/api/tauri.ts`.

1. Write failing UI tests for ONNX layout/model/classes/thresholds, positive/negative points, box and text prompts, progress/cancel/error, stale result rejection and one-step undo of an inference batch.
2. Add explicit AI panels/modes. Keep model downloads user-triggered, show missing Python/package/model feedback, and disable only AI controls while loading.
3. Apply a validated AI result to the editor core in one history operation with source/confidence metadata; prevent duplicates relative to existing objects.
4. Run tests/build and smoke test with available local runtime/model if present; commit task files.

## Task 8: Acceptance and parity matrix

**Files:** `docs/plans/2026-09-27-labelimg-parity-design.md` (status appendix), verification artifacts only.

1. Run `npm test`, `npm run build`, `cargo test` in `src-tauri`, and `git diff --check`.
2. Manually smoke test native Tauri with one YOLO and one VOC project: create/edit/undo/save/reopen, import/export, AI with a local test model if available. Do not alter the user's reference `labelImg` project.
3. Record which AI modes were validated with real dependencies/models and which used deterministic bridge fixtures. Resolve every gap in the design matrix before calling parity complete.
