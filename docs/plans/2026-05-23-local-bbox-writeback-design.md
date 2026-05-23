# Local BBox Writeback Design

## Goal

Implement a labelImg-like minimum workflow for detection annotation:

- Open a local image dataset directly.
- Support BBox annotation only in the first phase.
- Read and write Pascal VOC XML in place.
- Read and write YOLO detection TXT in place.
- Keep the existing workspace/project index and revision history as the app's internal state.

Later work can add full import/export workflows, polygon segmentation, and broader QA features.

## Current Context

The app is already a Tauri + React desktop application. It has:

- Dataset/project listing and local project indexing.
- An annotation workspace with image display, BBox drawing, selecting, dragging, resizing, deleting, duplicating, coordinate editing, and save/submit actions.
- VOC XML parsing and XML generation.
- YOLO detection and segmentation parsing.
- SQLite-backed image, class, annotation, snapshot, export, and task records.
- Browser fallback through the local Rust HTTP backend.

The missing first-phase pieces are mostly around local YOLO writeback, explicit format workflow, class selection, and save feedback.

## Recommended Approach

Reuse the existing annotation workspace and backend repository. A local dataset opened from disk is represented by a lightweight workspace project, but image files remain in their source directory.

For local-linked projects:

- The project manifest stores the source root and format.
- Image assets are resolved from the source root.
- Existing annotation files are parsed from the source root.
- Saving persists the internal JSON/SQLite state and writes the target VOC or YOLO sidecar file back to the source tree.

This reaches a useful labelImg-like workflow fastest while preserving the app's project model.

## Data Layout

### Pascal VOC

For an image path such as:

```text
dataset/subdir/a.jpg
```

the sidecar path is:

```text
dataset/subdir/a.xml
```

Objects are saved as `object/name` and `object/bndbox` entries. Existing VOC attributes such as `difficult`, `truncated`, and `confidence` remain best-effort attributes on objects.

### YOLO Detect

The first phase supports YOLO detection labels only:

```text
class_id x_center y_center width height
```

Values are normalized to image width and height.

The writer should prefer a conventional `labels` tree when the image lives under an `images` tree:

```text
dataset/images/train/a.jpg
dataset/labels/train/a.txt
```

If there is no `images` path segment, use a sidecar text file next to the image:

```text
dataset/subdir/a.jpg
dataset/subdir/a.txt
```

Class ids come from the project's stored class table. If an edited object label is not present, the backend should append or resolve it deterministically before writing YOLO.

## Frontend Workflow

The data submit dialog remains the entry point for local datasets:

- User enters or selects a local path.
- User chooses `VOC BBox` or `YOLO BBox`.
- The app calls `open_local_dataset(sourcePath, datasetType)`.
- The user enters the annotation workspace from the dataset card.

The annotation workspace should remain focused:

- BBox is the primary drawing mode.
- Existing polygon tools are hidden or disabled for first-phase local BBox projects.
- The inspector provides label editing, coordinate editing, duplicate, and delete.
- Save feedback states whether the source sidecar was written, for example `已写回 VOC XML` or `已写回 YOLO TXT`.

## Backend Workflow

Opening a local dataset indexes images and classes:

- VOC reads labels from existing XML files.
- YOLO reads labels from `classes.txt`, `obj.names`, `data.yaml`, or existing label ids when available.
- Unknown classes fall back to stable labels such as `class_0`.

Reading annotations checks, in order:

1. SQLite/internal JSON revision.
2. Native sidecar JSON.
3. Source format sidecar: VOC XML or YOLO TXT.
4. Empty object list.

Saving annotations:

1. Persists SQLite/internal JSON revision.
2. Writes native JSON.
3. If the project is local VOC, writes source XML.
4. If the project is local YOLO detect, writes source TXT.
5. Updates image status to draft/annotated consistently.

## Error Handling

Path and format failures should be explicit:

- Missing local root: show the backend error from `open_local_dataset`.
- Missing image file: show annotation workspace load failure.
- Sidecar write failure: fail the save call rather than silently saving internal state only.
- Revision conflicts: preserve the existing conflict behavior and ask the user to reload before saving again.

## Tests

Backend tests:

- Open a local VOC folder, read an existing XML object, save an edited box, and verify the XML changes.
- Open a local YOLO detect folder, read existing TXT objects, save an edited box, and verify normalized TXT output.
- Verify local image ids remain stable for nested paths.
- Verify YOLO sidecar path selection for `images/...` and non-standard folders.

Frontend tests:

- Data submit dialog sends `voc-detect` and `yolo-detect` local open requests.
- The annotation workspace saves edited BBox objects.
- The UI shows writeback-oriented save feedback for local VOC/YOLO projects.

## Non-goals

The first phase does not implement:

- Polygon or segmentation editing.
- Mask/keypoint tools.
- Full COCO export fidelity.
- LabelImg shortcut parity beyond the core BBox flow.
- Bulk validation or dataset split management.
