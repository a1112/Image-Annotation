# Class Sample Management Design

## Goal

Add a project-level category sample view so users can open a class and inspect the images that contain that class.

## Design

The project `类别` tab remains the entry point. Each class row gains a `查看样本` action. Selecting a class reveals a sample management area in the same tab, showing the class label, total matched images for the current page, total matched objects for the current page, and a paged image grid.

Samples reuse the existing image tile and read-only annotation overlay patterns from the `图片` tab. Each sample card shows the thumbnail, existing annotations, image status, and how many objects on that image match the selected class. The user can preview the image with the existing image preview dialog or open the independent annotation window for that image.

The backend owns class filtering. A new command, `list_class_samples`, accepts `projectId`, a class label, optional `classId`, `offset`, and `limit`. It reads indexed project images and annotation state, filters images that contain at least one matching object, and returns each matching `DatasetImage` with `matchCount`. Matching prefers `classId` when supplied and falls back to label comparison so older data remains usable.

The frontend adds a `ClassSample` type and `listClassSamples` API wrapper. `ProjectWorkspace` tracks the selected class and class sample page, loads samples when the selected class or page changes, and uses existing asset URL and annotation hooks against the sample images. Page size matches the project image grid.

## Error Handling

If class sample loading fails because the backend is unavailable, the project-level backend unavailable panel remains the top-level error surface. If only the class sample request fails, the class tab shows a compact error message and keeps the class list visible so the user can choose another class or retry by reselecting the class.

If a class has no matched images, the sample area shows an empty state instead of an empty grid.

## Testing

Add React tests for:

- Opening the project `类别` tab and clicking `查看样本`.
- Calling `list_class_samples` with the selected class label and pagination.
- Rendering matched image cards with match counts, preview action, and annotation overlay.
- Opening the existing image preview dialog from a class sample.

Add Rust repository tests for:

- Returning only images that contain the selected class.
- Counting multiple matching objects on the same image.
- Falling back to label matching when class ids do not match.

Keep existing image browsing and annotation tests unchanged.
