# Annotation Workbench Visual Cleanup Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Clean up the annotation workbench UI so it looks more organized and professional while preserving all existing annotation behavior.

**Architecture:** This is primarily a CSS refinement in `src/styles.css`. The React structure already exposes the needed workbench hooks: toolbar, action row, canvas shell, filmstrip, inspector, object rows, and bbox editor.

**Tech Stack:** React, TypeScript, CSS, Vite, Vitest.

---

### Task 1: Clean Workbench Chrome

**Files:**
- Modify: `src/styles.css`

**Steps:**
1. Tighten `.annotation-page`, `.tool-rail`, `.workspace-area`, and `.annotation-toolbar`.
2. Make toolbar controls consistent at 32px height.
3. Keep desktop three-column layout and narrow-screen inspector behavior.
4. Run `npm run build`.

### Task 2: Clean Canvas And Filmstrip

**Files:**
- Modify: `src/styles.css`

**Steps:**
1. Make the canvas shell the strongest visual surface.
2. Use a muted canvas background and compact floating zoom control.
3. Reduce filmstrip height, crop thumbnails consistently, and prevent filename crowding.
4. Verify in the browser at `http://127.0.0.1:1440/#/annotate/coco128/000000000009`.

### Task 3: Clean Inspector

**Files:**
- Modify: `src/styles.css`

**Steps:**
1. Make object rows compact and table-like.
2. Improve property labels, inputs, and bbox editor spacing.
3. Run `npm test -- --run src/App.test.tsx` and `npm run build`.
4. Use browser screenshot inspection for desktop and narrow widths.

