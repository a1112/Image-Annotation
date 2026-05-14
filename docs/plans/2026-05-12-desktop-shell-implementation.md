# Desktop Shell Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the initial React + Tauri desktop shell for the Image Annotation app, including frameless window behavior, tray support, dataset-card home page, and annotation workspace preview.

**Architecture:** Use Tauri v2 for native window/tray commands and React/Vite for the desktop UI. Reuse the proven window and tray patterns from `G:\Project\WindowBase`, but rename and simplify them for this app.

**Tech Stack:** React, TypeScript, Vite, Tauri v2, Rust, Vitest, Testing Library.

---

### Task 1: Project Scaffold

**Files:**
- Create: `package.json`
- Create: `index.html`
- Create: `vite.config.ts`
- Create: `tsconfig.json`
- Create: `src-tauri/Cargo.toml`
- Create: `src-tauri/tauri.conf.json`

**Steps:**
1. Add package and build configuration for React + Vite + Tauri.
2. Add Tauri v2 configuration with a transparent frameless main window.
3. Keep dev server port aligned with `WindowBase` style.

### Task 2: Desktop Backend

**Files:**
- Create: `src-tauri/src/main.rs`
- Create: `src-tauri/src/lib.rs`
- Create: `src-tauri/src/platform.rs`
- Create: `src-tauri/build.rs`
- Create: `src-tauri/capabilities/default.json`

**Steps:**
1. Write Rust tests for tray menu action mapping.
2. Implement window commands: drag, minimize, maximize, hide to tray, show, close.
3. Implement tray menu: Show, Hide to Tray, Start Annotation, Export, Quit.
4. Configure the main window on setup and close-to-tray behavior.

### Task 3: React UI

**Files:**
- Create: `src/App.tsx`
- Create: `src/App.test.tsx`
- Create: `src/main.tsx`
- Create: `src/styles.css`

**Steps:**
1. Write React tests for dataset cards, tag-group summaries, and annotation workspace navigation.
2. Implement a frameless shell with top command bar and icon-only rail.
3. Implement card-based dataset management home page.
4. Implement annotation workspace preview based on the accepted layout.
5. Wire UI window controls to Tauri commands with web fallback safety.

### Task 4: Verification

**Commands:**
- `npm install`
- `npm test -- --run`
- `npm run build`
- `cargo test --manifest-path src-tauri/Cargo.toml`

