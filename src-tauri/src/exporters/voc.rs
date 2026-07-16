use super::{copy_image, export_file_name, normalized_objects, ExportOptions, SnapshotData};
use crate::importers::voc;
use std::{fs, path::Path};

pub(super) fn export(
    snapshot: &SnapshotData,
    source_root: &Path,
    output_root: &Path,
    options: &ExportOptions,
) -> Result<(), String> {
    let image_dir = output_root.join("JPEGImages");
    let annotation_dir = output_root.join("Annotations");
    fs::create_dir_all(&annotation_dir).map_err(|err| err.to_string())?;
    for image in &snapshot.images {
        let exported_image = if options.include_images {
            copy_image(image, source_root, &image_dir)?
        } else {
            fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
            image_dir.join(export_file_name(image))
        };
        let objects = normalized_objects(&image.objects, options.polygon_policy.as_deref(), false);
        let xml =
            voc::annotations_to_voc_xml(&exported_image, image.width, image.height, &objects)?;
        fs::write(annotation_dir.join(format!("{}.xml", image.image_id)), xml)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}
