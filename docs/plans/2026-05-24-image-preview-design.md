# Image Preview Design

## Goal

Improve the project image tab by adding a focused image preview dialog before deeper annotation work.

## Design

The `图片` tab remains the image browsing surface. Each image tile gains an explicit preview action while preserving the existing double-click and Enter behavior that opens the annotation workspace. The tab header is adjusted so the title and supporting copy align at the top-left, with paging controls aligned at the top-right.

The preview dialog is an application modal. It shows the selected image at a larger size, renders the existing annotation overlay in read-only mode, and displays key metadata: filename, size, status, split, tags, and object count. The primary action is `标记`, which requests the independent annotation console for the current project and image. In desktop mode, the existing project console is focused and navigated to the requested image; if no console exists, a new one is created. In browser-only fallback mode, the action navigates to `#/annotate/{projectId}/{imageId}`.

The annotation workspace route renders as an independent interface. It does not include the main dataset shell, topbar, or primary navigation rail. Because the Tauri annotation window is frameless, the workspace toolbar owns its own minimize, maximize, and close controls.

## Testing

Add focused React tests for:

- Opening a preview dialog from the project `图片` tab.
- Rendering the selected image, metadata, and annotation overlay inside the dialog.
- Clicking `标记` from the dialog and landing in the existing annotation workspace.

The implementation should keep backend APIs unchanged and reuse existing image URL and annotation loading hooks.
