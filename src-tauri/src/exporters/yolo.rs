use super::{copy_image, export_file_name, normalized_objects, ExportOptions, SnapshotData};
use crate::importers::yolo;
use std::{fs, path::Path};

pub(super) fn export(
    snapshot: &SnapshotData,
    source_root: &Path,
    output_root: &Path,
    options: &ExportOptions,
    segmentation: bool,
) -> Result<(), String> {
    for image in &snapshot.images {
        let split = if image.split.is_empty() {
            "train"
        } else {
            &image.split
        };
        let image_dir = output_root.join("images").join(split);
        if options.include_images {
            copy_image(image, source_root, &image_dir)?;
        } else {
            fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
        }
        let label_dir = output_root.join("labels").join(split);
        fs::create_dir_all(&label_dir).map_err(|err| err.to_string())?;
        let objects = normalized_objects(
            &image.objects,
            options.polygon_policy.as_deref(),
            segmentation,
        );
        let data = if segmentation {
            yolo::annotations_to_yolo_polygon_lines(&objects, image.width, image.height)?
        } else {
            yolo::annotations_to_yolo_lines(&objects, image.width, image.height)?
        };
        let label_name = Path::new(&export_file_name(image)).with_extension("txt");
        fs::write(label_dir.join(label_name), data).map_err(|err| err.to_string())?;
    }
    fs::write(
        output_root.join("classes.txt"),
        snapshot.classes.join("\n") + "\n",
    )
    .map_err(|err| err.to_string())?;
    let names = snapshot
        .classes
        .iter()
        .map(|label| format!("'{}'", label.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        output_root.join("data.yaml"),
        format!("path: .\ntrain: images/train\nval: images/val\nnames: [{names}]\n"),
    )
    .map_err(|err| err.to_string())
}
