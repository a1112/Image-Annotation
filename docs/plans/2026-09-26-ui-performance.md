# UI Performance and Window Controls Implementation Plan

> Execute sequentially in the current checkout so the running desktop app receives the fixes.

**Goal:** Restore native window buttons and reduce avoidable UI requests and canvas work while retaining the current visual layout.

**Architecture:** Keep current React/Tauri commands; fix SVG drag hit testing, request preview data according to the visible project tab, and coalesce canvas painting to animation frames. Do not cache mutable annotations across project/page changes.

**Tech Stack:** React 18, TypeScript, Vitest, Tauri 2.

## 1. Window button regression
- Add tests in src/App.test.tsx that press the SVG/path within window controls and toolbar actions, asserting no start_drag_window call and the correct button command.
- Run the tests before editing src/App.tsx; confirm they fail on SVG mouse-down.
- Handle Element (including SVGElement) in beginDesktopWindowDrag, retaining passive-titlebar drag behavior.
- Verify native maximize/restore and minimize using the running application.

## 2. Preview request budgets
- Add request-count tests for overview, non-image tabs and image browsing.
- Overview needs 6 URLs and 12 annotation records; image tabs and preview dialogs need the current page; non-image tabs need none.
- Class samples load assets/annotations only while visible or previewed.
- Direct annotation and task windows should not load homepage dataset previews. Load homepage data on the homepage or when opening data import.
- Retain actual data and verify navigation and preview behavior with existing tests.

## 3. Canvas scheduling
- Add a fake-animation-frame regression that checks multiple updates before a frame produce one paint with the latest zoom, and unmount cancels pending paint.
- Schedule drawAnnotationCanvas through requestAnimationFrame and cancel superseded/unmounted work.
- Skip coordinate/layout reads on idle mouse movement.

## 4. Verification
- Restrict Vitest discovery to src tests (other .worktrees are not this checkout's test suite).
- Run npm test and npm run build.
- Verify real COCO128 overview, image preview and annotation canvas in browser/desktop.
- Record request budgets and actual native window behavior. Do not infer FPS improvements from request counts or mock tests.

## Results (2026-09-26)
- Fixed SVG/path hit testing so window buttons do not trigger native titlebar dragging. Regression coverage includes minimize, maximize and close-to-tray commands.
- Verified native maximize and minimize in the running Tauri application. Restored through the Windows system menu. A separate desktop upload overlay covers the top-right buttons when maximized, so custom-button restore was not independently verified in that state. Native close-to-tray was not exercised; its command dispatch is covered by the regression test.
- Overview request-count regression: asset requests reduced from 48 to 6 and annotation requests from 48 to 12. Image browsing still loads the full page; hidden snapshot tabs load neither. These are test request counts, not measured timing or FPS.
- Canvas regression verifies eight zoom updates produce one paint using the latest state, and unmount cancels a queued paint. Idle pointer movement skips unnecessary layout reads.
- Standalone annotation windows skip homepage previews. Opening/closing the homepage import dialog no longer reloads all datasets or retriggers automatic download.
- Browser smoke check: a real COCO128 preview loaded at 640 × 480 and the annotation canvas rendered the image at 125% zoom. No annotations were saved or changed.
- Final verification: `npm test` passed 86 Vitest tests and 12 window-chrome tests; `npm run build` passed; `git diff --check` passed.
- Native screenshot: [restored window](../verification/2026-09-26/window-restored.jpg).
