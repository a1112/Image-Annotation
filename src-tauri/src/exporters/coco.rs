use super::{copy_image, export_file_name, polygon_bounds, ExportOptions, SnapshotData};
use serde_json::{json, Map, Value};
use std::{fs, path::Path};

pub(super) fn export(
    snapshot: &SnapshotData,
    source_root: &Path,
    output_root: &Path,
    options: &ExportOptions,
) -> Result<(), String> {
    let image_dir = output_root.join("images");
    let categories = snapshot
        .classes
        .iter()
        .enumerate()
        .map(|(index, label)| json!({"id": index + 1, "name": label}))
        .collect::<Vec<_>>();
    let mut images = Vec::new();
    let mut annotations = Vec::new();
    let mut annotation_id = 1_u64;
    for (image_index, image) in snapshot.images.iter().enumerate() {
        if options.include_images {
            copy_image(image, source_root, &image_dir)?;
        } else {
            fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
        }
        let image_id = (image_index + 1) as u64;
        images.push(json!({
            "id": image_id,
            "file_name": format!("images/{}", export_file_name(image)),
            "width": image.width,
            "height": image.height,
        }));
        for object in &image.objects {
            let category_id = object.class_id as u64 + 1;
            let iscrowd = object
                .attributes
                .get("coco.iscrowd")
                .cloned()
                .unwrap_or_else(|| json!(0));
            let mut extra = object
                .attributes
                .get("coco.rawAnnotation")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(Map::new);
            for reserved in [
                "id",
                "image_id",
                "category_id",
                "bbox",
                "segmentation",
                "area",
                "iscrowd",
                "keypoints",
            ] {
                extra.remove(reserved);
            }
            let mut annotation = Map::new();
            annotation.insert("id".to_string(), json!(annotation_id));
            annotation.insert("image_id".to_string(), json!(image_id));
            annotation.insert("category_id".to_string(), json!(category_id));
            annotation.insert("iscrowd".to_string(), iscrowd);
            if let Some(bbox) = &object.bbox {
                annotation.insert(
                    "bbox".to_string(),
                    json!([bbox.x, bbox.y, bbox.width, bbox.height]),
                );
                annotation.insert("area".to_string(), json!(bbox.width * bbox.height));
            } else if let Some(polygon) = &object.polygon {
                let bounds = polygon_bounds(polygon)
                    .ok_or_else(|| format!("polygon object {} has no points", object.id))?;
                annotation.insert(
                    "bbox".to_string(),
                    json!([bounds.x, bounds.y, bounds.width, bounds.height]),
                );
                annotation.insert(
                    "segmentation".to_string(),
                    Value::Array(vec![Value::Array(
                        polygon
                            .iter()
                            .flat_map(|point| [json!(point.x), json!(point.y)])
                            .collect(),
                    )]),
                );
                annotation.insert("area".to_string(), json!(polygon_area(polygon)));
            }
            annotation.extend(extra);
            annotations.push(Value::Object(annotation));
            annotation_id += 1;
        }
    }
    let data = serde_json::to_string_pretty(&json!({
        "info": {"description": snapshot.name},
        "licenses": [],
        "images": images,
        "annotations": annotations,
        "categories": categories,
    }))
    .map_err(|err| err.to_string())?;
    fs::write(output_root.join("annotations.json"), data).map_err(|err| err.to_string())
}

fn polygon_area(points: &[crate::domain::Point]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for index in 0..points.len() {
        let current = &points[index];
        let next = &points[(index + 1) % points.len()];
        area += current.x * next.y - next.x * current.y;
    }
    area.abs() / 2.0
}
