# Single Dataset Workspace Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the sparse single-dataset overview with a production-oriented workspace that exposes progress, actionable queues, recent samples, class distribution, and dataset metadata.

**Architecture:** Keep `ProjectWorkspace` as the data owner and introduce a focused overview renderer fed entirely by the existing `ProjectDetail`, image page, asset URL, and workflow callbacks. Extend only frontend markup and CSS; preserve all backend contracts and existing tab implementations.

**Tech Stack:** React 18, TypeScript, Lucide React, CSS Grid, Vitest, Testing Library.

---

### Task 1: Specify Overview Behavior

**Files:**
- Modify: `src/App.test.tsx`

1. Add a test that opens COCO128 and asserts production progress, actionable queue, recent samples, class distribution, and dataset metadata.
2. Add a test that uses overview shortcuts to switch to images and a selected class sample view.
3. Run `npm test -- --run src/App.test.tsx` and confirm the new assertions fail because the workspace does not exist.

### Task 2: Build the Production Overview

**Files:**
- Modify: `src/App.tsx`

1. Add semantic icons to the existing project tab model.
2. Pass preview, tab switching, annotation, and class-selection callbacks into the overview renderer.
3. Render progress, work queue, recent samples, class distribution, and technical metadata from existing project data.
4. Re-run the focused tests and make them pass without changing backend mocks or contracts.

### Task 3: Apply Responsive Operational Styling

**Files:**
- Modify: `src/styles.css`
- Test: `src/styles.test.ts`

1. Add a failing style contract test for responsive overview grids and stable sample dimensions.
2. Implement full-width workspace bands, progress visuals, queue rows, sample strip, class bars, and responsive collapse rules.
3. Run focused style and component tests.

### Task 4: Verify Runtime Quality

**Files:**
- Modify only if visual defects are found: `src/App.tsx`, `src/styles.css`

1. Run `npm test -- --run`.
2. Run `npm run build`.
3. Open `#/datasets/coco128` in the running browser and capture desktop and narrow screenshots.
4. Inspect for overlap, clipping, blank assets, broken controls, and console errors; fix and repeat until clean.

