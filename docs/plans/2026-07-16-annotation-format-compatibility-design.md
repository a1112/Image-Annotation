# Annotation Format Compatibility Design

## Goal

Build a complete local dataset management workflow for the formats most commonly
used by image annotation tools:

- YOLO detection TXT.
- YOLO segmentation TXT.
- Pascal VOC detection XML.
- COCO detection and polygon segmentation JSON.
- LabelMe rectangle and polygon JSON.
- Image-only directories for unannotated data.

The application must detect these formats, preview import quality, index them into
one internal model, support editing without losing supported metadata, write back
to local sources where practical, and export valid datasets rather than placeholder
files.

## Current Context

The application already has:

- Tauri and standalone Rust backend entry points.
- Filesystem-backed projects with SQLite indexes.
- Local-linked projects that keep images in their original directory.
- Copy-import projects under the workspace directory.
- BBox and polygon annotation objects in the editor.
- Pascal VOC BBox parsing and writing.
- YOLO BBox and polygon parsing, plus YOLO BBox writing.
- Dataset source analysis, a confirmation dialog, import history, snapshots, and
  export records.

The main gaps are:

- Detection only recognizes VOC, YOLO detection, and image directories.
- YOLO segmentation is not reliably detected or written back.
- COCO and LabelMe have no real import path.
- Source annotation identity and format-specific metadata are not persisted.
- Export currently writes placeholder COCO/YOLO files.
- Validation cannot distinguish warnings from errors that must block import.

## Recommended Architecture

Use a canonical internal annotation model and isolate each external format behind
an adapter.

Each adapter owns:

1. Format detection.
2. Dataset inspection and validation.
3. Import into canonical records.
4. Source synchronization where supported.
5. Export from canonical records.

The dataset service orchestrates adapters but does not parse format-specific files.
The annotation repository reads and writes canonical records and asks the adapter
to synchronize external source files.

This avoids converting all formats through YOLO, which would lose metadata, and
avoids duplicating project, revision, task, and export logic for every format.

## Canonical Model

### Dataset Source

Persist the following per project:

- `source_format`: `yolo-detect`, `yolo-seg`, `voc-detect`, `coco`, `labelme`, or
  `image-directory`.
- `source_mode`: `linked` or `copied`.
- `source_root`: canonical source directory.
- `source_annotation_path`: centralized annotation file when applicable.
- `source_options_json`: adapter-specific dataset settings.

### Image Source Mapping

Persist the following per image:

- Internal stable image id.
- Relative image path.
- External image id, such as COCO numeric image id.
- Source annotation path for sidecar formats.
- Split.
- Source checksum or modified timestamp for conflict detection.

### Annotation Object

Continue using BBox and Polygon as editable object types, and extend persisted
attributes to retain supported external metadata:

- External annotation id.
- COCO `iscrowd`, `area`, and extra attributes.
- VOC `difficult`, `truncated`, `pose`, and confidence.
- LabelMe `group_id`, `flags`, and shape flags.
- Unknown serializable properties under a namespaced raw-attributes field.

Mask, keypoint, and track objects are not editable in this phase. Inspection must
report them explicitly as skipped unsupported objects. They must never disappear
silently.

## Adapter Contract

Create a common adapter interface with these operations:

- `detect`: score whether selected files represent the format.
- `inspect`: return images, classes, object counts, problems, and a preview tree.
- `import`: write canonical project/image/class/annotation/source mapping records.
- `load_image_annotations`: parse source data for one image when canonical state
  has not been edited.
- `sync_image` or `sync_dataset`: update source annotations.
- `export`: write a complete dataset in the selected format.

Detection uses positive format signatures rather than file extension alone. For
example, LabelMe and COCO are both JSON but have different required structures.

## Format Behavior

### YOLO Detection

- Detect lines containing a class id and four normalized numbers.
- Read class names from `data.yaml`, `classes.txt`, or `obj.names`.
- Resolve conventional `images/...` to `labels/...` trees.
- Write edited BBox objects to source TXT in linked mode.
- Export images, labels, and `data.yaml`.

### YOLO Segmentation

- Detect lines containing a class id and at least three normalized points.
- Reject mixed malformed lines and report the exact file and line.
- Read and write polygon objects.
- Export images, polygon labels, and `data.yaml`.

### Pascal VOC

- Detect XML with an `annotation` root, image size, and object/bndbox entries.
- Resolve XML beside images and common `Annotations` plus `JPEGImages` layouts.
- Read and write BBox annotations.
- Preserve supported object attributes.
- Export one XML per image with an image directory.

### COCO

- Detect JSON containing `images`, `annotations`, and `categories`.
- Support BBox and polygon segmentation arrays.
- Resolve image paths using `file_name` relative to the selected root.
- Persist numeric image, category, and annotation ids for round trips.
- Save edits canonically; synchronize linked source through an explicit
  dataset-level rewrite because one JSON contains many images.
- Export complete COCO JSON with valid categories, images, annotations, bbox,
  segmentation, area, and `iscrowd`.

RLE masks, keypoints, captions, and panoptic records are reported as unsupported
and skipped.

### LabelMe

- Detect JSON containing `imagePath` and `shapes`.
- Convert `rectangle` shapes to BBox and `polygon` shapes to Polygon.
- Resolve embedded `imageData` only for inspection; copied import may materialize
  it into an image file.
- Write edited sidecar JSON in linked mode.
- Preserve flags, group ids, and supported shape attributes.
- Export one JSON file per image.

### Image Directory

- Index supported image files as unannotated samples.
- Permit later export into any supported target format.

## Import Workflow

The data-add dialog remains a two-stage workflow:

1. Select or drop a folder or files.
2. Inspect and confirm.

Inspection displays:

- Detected format and confidence.
- Images, annotations, classes, splits, and unsupported object counts.
- Missing images or annotation files.
- Duplicate image ids or paths.
- Invalid coordinates, malformed records, and unknown classes.
- Source tree and centralized annotation file.

Problems have severity:

- `error`: import cannot safely proceed.
- `warning`: import can proceed after explicit confirmation.
- `info`: descriptive detail.

The user can override the detected format when detection is ambiguous. The
backend validates the override before import.

## Linked And Copied Modes

Linked mode:

- Keeps source images and annotation files in place.
- Stores canonical indexes, edit history, and source mappings in the workspace.
- Sidecar formats synchronize per image.
- COCO synchronizes through an explicit dataset-level source sync.
- Detects source changes before overwriting and reports conflicts.

Copied mode:

- Copies the complete supported dataset structure into the workspace.
- Treats copied files as the new source of truth.
- Uses the same adapters and synchronization behavior.

## Export Workflow

Exports operate from a snapshot and canonical records, not by copying source
annotation files blindly.

Supported targets:

- YOLO Detect.
- YOLO Seg.
- Pascal VOC.
- COCO.
- LabelMe.

An export validates object compatibility first. For example, exporting polygons to
YOLO Detect requires the user to choose whether polygons are converted to bounding
boxes or skipped. Default behavior is to block lossy export.

Each export record stores:

- Target format.
- Output directory.
- Snapshot id.
- Image and annotation counts.
- Warning/error summary.
- Machine-readable manifest.

## Error And Conflict Handling

- Parsing errors include source path, record or line number, and a user-facing
  explanation.
- Missing images and duplicate external ids block centralized formats.
- Invalid individual sidecars are reported without hiding valid files.
- Linked source writes use temporary files followed by atomic replacement.
- Source checksum or modified-time changes create a conflict instead of
  overwriting external edits.
- Internal revision conflicts retain the existing optimistic revision behavior.
- Failed source synchronization does not claim that the source file was updated.

## Testing Strategy

Add small fixtures for each supported format:

- YOLO Detect with nested train/val images and labels.
- YOLO Seg with polygons.
- Pascal VOC sidecars and common `Annotations/JPEGImages` layout.
- COCO with BBox and polygon objects.
- LabelMe rectangle and polygon sidecars.
- Malformed and partially missing datasets.

For every format, verify:

1. Detection.
2. Inspection counts and problems.
3. Import and source mapping.
4. Annotation loading.
5. Edit and synchronization behavior.
6. Export.
7. Re-import of the export.
8. Equivalent supported geometry, classes, paths, and retained metadata.

Frontend tests verify format display, problem severity, confirmation blocking,
mode selection, sync actions, and export choices.

## Delivery Order

1. Canonical source mapping and inspection problem model.
2. Adapter registry and YOLO Detect/VOC migration.
3. YOLO Seg round trip.
4. LabelMe round trip.
5. COCO import, export, and explicit source synchronization.
6. Unified export UI and validation.
7. Full fixture matrix and runtime verification.

## Non-goals

This phase does not add editable:

- Bitmap or RLE masks.
- Keypoints or skeletons.
- Video tracks.
- Panoptic segmentation.
- DICOM-specific metadata.

These records are identified and reported so later adapters can extend the
canonical model without changing the import workflow.
