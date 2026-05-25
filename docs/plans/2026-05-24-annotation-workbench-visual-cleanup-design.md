# Annotation Workbench Visual Cleanup Design

Goal: make the annotation workbench cleaner, denser, and easier to scan without changing annotation behavior.

Approved direction:
- Keep the workbench as a tool-like interface, not a decorative landing page.
- Use the canvas as the visual center.
- Reduce toolbar visual noise and make controls consistent.
- Make the inspector read like a compact object/property panel.
- Keep responsive behavior: stable three-column desktop layout, inspector below the canvas on narrow screens.

Implementation notes:
- Prefer CSS-only changes.
- Only adjust JSX if a class hook is required.
- Preserve existing tests, shortcuts, image preview, canvas drawing, save, and window drag behavior.

