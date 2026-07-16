use super::{copy_image, export_file_name, ExportOptions, SnapshotData};
use serde_json::{json, Map, Value};
use std::{fs, path::Path};

pub(super) fn export(
    snapshot: &SnapshotData,
    source_root: &Path,
    output_root: &Path,
    options: &ExportOptions,
) -> Result<(), String> {
    let image_dir = output_root.join("images");
    fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
    for image in &snapshot.images {
        if options.include_images {
            copy_image(image, source_root, &image_dir)?;
        }
        let shapes = image
            .objects
            .iter()
            .filter_map(|object| {
                let (shape_type, points) = if let Some(bbox) = &object.bbox {
                    (
                        "rectangle",
                        json!([
                            [bbox.x, bbox.y],
                            [bbox.x + bbox.width, bbox.y + bbox.height]
                        ]),
                    )
                } else {
                    let polygon = object.polygon.as_ref()?;
                    (
                        "polygon",
                        Value::Array(
                            polygon
                                .iter()
                                .map(|point| json!([point.x, point.y]))
                                .collect(),
                        ),
                    )
                };
                let mut shape = Map::new();
                shape.insert("label".to_string(), json!(object.label));
                shape.insert("points".to_string(), points);
                shape.insert(
                    "group_id".to_string(),
                    object
                        .attributes
                        .get("labelme.groupId")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                shape.insert("shape_type".to_string(), json!(shape_type));
                shape.insert(
                    "flags".to_string(),
                    object
                        .attributes
                        .get("labelme.shapeFlags")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                );
                Some(Value::Object(shape))
            })
            .collect::<Vec<_>>();
        let data = serde_json::to_string_pretty(&json!({
            "version": "5.0.1",
            "flags": {},
            "shapes": shapes,
            "imagePath": export_file_name(image),
            "imageData": null,
            "imageHeight": image.height,
            "imageWidth": image.width,
        }))
        .map_err(|err| err.to_string())?;
        fs::write(image_dir.join(format!("{}.json", image.image_id)), data)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}
