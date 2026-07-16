mod coco;
mod labelme;
mod voc;
mod yolo;

use crate::domain::{AnnotationObject, BBox, Point};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportOptions {
    pub format: String,
    #[serde(default)]
    pub polygon_policy: Option<String>,
    #[serde(default = "default_include_images")]
    pub include_images: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotData {
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub classes: Vec<String>,
    #[serde(default, alias = "annotations")]
    pub images: Vec<SnapshotImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotImage {
    pub image_id: String,
    pub file_name: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default = "default_split")]
    pub split: String,
    #[serde(default)]
    pub objects: Vec<AnnotationObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub format: String,
    pub image_count: u32,
    pub annotation_count: u32,
    pub class_count: u32,
    pub problems: Vec<ExportProblem>,
    pub options: ExportOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProblem {
    pub severity: String,
    pub code: String,
    pub message: String,
}

pub fn export_snapshot(
    snapshot: &SnapshotData,
    source_root: &Path,
    output_root: &Path,
    options: &ExportOptions,
) -> Result<ExportManifest, String> {
    validate_export(snapshot, options)?;
    fs::create_dir_all(output_root).map_err(|err| err.to_string())?;
    match normalize_format(&options.format) {
        "yolo-detect" => yolo::export(snapshot, source_root, output_root, options, false)?,
        "yolo-seg" => yolo::export(snapshot, source_root, output_root, options, true)?,
        "voc-detect" => voc::export(snapshot, source_root, output_root, options)?,
        "coco" => coco::export(snapshot, source_root, output_root, options)?,
        "labelme" => labelme::export(snapshot, source_root, output_root, options)?,
        format => return Err(format!("unsupported export format: {format}")),
    }
    let manifest = ExportManifest {
        format: normalize_format(&options.format).to_string(),
        image_count: snapshot.images.len() as u32,
        annotation_count: snapshot
            .images
            .iter()
            .map(|image| image.objects.len() as u32)
            .sum(),
        class_count: snapshot.classes.len() as u32,
        problems: Vec::new(),
        options: options.clone(),
    };
    let data = serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
    fs::write(output_root.join("export-manifest.json"), data).map_err(|err| err.to_string())?;
    Ok(manifest)
}

fn validate_export(snapshot: &SnapshotData, options: &ExportOptions) -> Result<(), String> {
    let format = normalize_format(&options.format);
    let has_polygons = snapshot
        .images
        .iter()
        .flat_map(|image| &image.objects)
        .any(|object| object.polygon.is_some());
    if has_polygons && matches!(format, "yolo-detect" | "voc-detect") {
        match options.polygon_policy.as_deref() {
            Some("bbox" | "skip") => {}
            _ => {
                return Err(format!(
                    "{format} export contains polygons; set polygonPolicy to 'bbox' or 'skip'"
                ));
            }
        }
    }
    for image in &snapshot.images {
        if image.width == 0 || image.height == 0 {
            return Err(format!(
                "image dimensions are required for export: {}",
                image.file_name
            ));
        }
    }
    Ok(())
}

fn normalized_objects(
    objects: &[AnnotationObject],
    polygon_policy: Option<&str>,
    polygons_required: bool,
) -> Vec<AnnotationObject> {
    objects
        .iter()
        .filter_map(|object| {
            if polygons_required {
                if object.polygon.is_some() {
                    return Some(object.clone());
                }
                let bbox = object.bbox.as_ref()?;
                let mut converted = object.clone();
                converted.object_type = "polygon".to_string();
                converted.polygon = Some(bbox_to_polygon(bbox));
                converted.bbox = None;
                return Some(converted);
            }
            if object.bbox.is_some() {
                return Some(object.clone());
            }
            match polygon_policy {
                Some("bbox") => {
                    let polygon = object.polygon.as_ref()?;
                    let mut converted = object.clone();
                    converted.object_type = "bbox".to_string();
                    converted.bbox = polygon_bounds(polygon);
                    converted.polygon = None;
                    Some(converted)
                }
                Some("skip") => None,
                _ => None,
            }
        })
        .collect()
}

fn copy_image(
    image: &SnapshotImage,
    source_root: &Path,
    target_dir: &Path,
) -> Result<PathBuf, String> {
    fs::create_dir_all(target_dir).map_err(|err| err.to_string())?;
    let direct = source_root.join(&image.file_name);
    let source = if direct.is_file() {
        direct
    } else {
        WalkDir::new(source_root)
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file()
                    && (entry.file_name().to_string_lossy() == image.file_name
                        || entry
                            .path()
                            .file_stem()
                            .map(|value| value.to_string_lossy() == image.image_id)
                            .unwrap_or(false))
            })
            .map(|entry| entry.path().to_path_buf())
            .unwrap_or(direct)
    };
    if !source.is_file() {
        return Err(format!("source image not found: {}", source.display()));
    }
    let target = target_dir.join(export_file_name(image));
    fs::copy(&source, &target).map_err(|err| err.to_string())?;
    Ok(target)
}

fn export_file_name(image: &SnapshotImage) -> String {
    let extension = Path::new(&image.file_name)
        .extension()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "jpg".to_string());
    format!("{}.{}", image.image_id, extension)
}

fn polygon_bounds(points: &[Point]) -> Option<BBox> {
    let min_x = points.iter().map(|point| point.x).reduce(f64::min)?;
    let min_y = points.iter().map(|point| point.y).reduce(f64::min)?;
    let max_x = points.iter().map(|point| point.x).reduce(f64::max)?;
    let max_y = points.iter().map(|point| point.y).reduce(f64::max)?;
    Some(BBox {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

fn bbox_to_polygon(bbox: &BBox) -> Vec<Point> {
    vec![
        Point {
            x: bbox.x,
            y: bbox.y,
        },
        Point {
            x: bbox.x + bbox.width,
            y: bbox.y,
        },
        Point {
            x: bbox.x + bbox.width,
            y: bbox.y + bbox.height,
        },
        Point {
            x: bbox.x,
            y: bbox.y + bbox.height,
        },
    ]
}

fn normalize_format(format: &str) -> &str {
    if format == "yolo" {
        "yolo-detect"
    } else {
        format
    }
}

fn default_include_images() -> bool {
    true
}

fn default_split() -> String {
    "train".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{AnnotationObject, BBox, Point},
        importers::{coco, labelme, voc, yolo},
    };
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn fixture() -> (PathBuf, SnapshotData) {
        let root = temp_root("exporters");
        fs::create_dir_all(root.join("source")).unwrap();
        image::RgbaImage::new(100, 80)
            .save(root.join("source").join("a.png"))
            .unwrap();
        image::RgbaImage::new(120, 90)
            .save(root.join("source").join("b.png"))
            .unwrap();
        let mut bbox = AnnotationObject::bbox(
            "bbox-1".to_string(),
            0,
            "defect".to_string(),
            BBox {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
        );
        bbox.attributes
            .insert("difficult".to_string(), serde_json::json!(true));
        let polygon = AnnotationObject::polygon(
            "polygon-1".to_string(),
            1,
            "scratch".to_string(),
            vec![
                Point { x: 5.0, y: 5.0 },
                Point { x: 50.0, y: 5.0 },
                Point { x: 45.0, y: 40.0 },
            ],
        );
        (
            root,
            SnapshotData {
                project_id: "fixture".to_string(),
                name: "fixture".to_string(),
                classes: vec!["defect".to_string(), "scratch".to_string()],
                images: vec![
                    SnapshotImage {
                        image_id: "a".to_string(),
                        file_name: "a.png".to_string(),
                        width: 100,
                        height: 80,
                        split: "train".to_string(),
                        objects: vec![bbox],
                    },
                    SnapshotImage {
                        image_id: "b".to_string(),
                        file_name: "b.png".to_string(),
                        width: 120,
                        height: 90,
                        split: "val".to_string(),
                        objects: vec![polygon],
                    },
                ],
            },
        )
    }

    #[test]
    fn exports_supported_formats_as_reimportable_files() {
        let (root, snapshot) = fixture();
        let source_root = root.join("source");

        let yolo_detect = root.join("yolo-detect");
        export_snapshot(
            &snapshot,
            &source_root,
            &yolo_detect,
            &ExportOptions {
                format: "yolo-detect".to_string(),
                polygon_policy: Some("bbox".to_string()),
                include_images: true,
            },
        )
        .unwrap();
        let yolo_text =
            fs::read_to_string(yolo_detect.join("labels").join("train").join("a.txt")).unwrap();
        let parsed_bbox = yolo::parse_yolo_bbox_line(yolo_text.trim(), 100, 80)
            .unwrap()
            .bbox;
        assert_eq!(parsed_bbox.x, 10.0);
        assert_eq!(parsed_bbox.y, 20.0);
        assert_eq!(parsed_bbox.width, 30.0);
        assert_eq!(parsed_bbox.height, 40.0);
        assert!(yolo_detect
            .join("labels")
            .join("val")
            .join("b.txt")
            .exists());

        let yolo_seg = root.join("yolo-seg");
        export_snapshot(
            &snapshot,
            &source_root,
            &yolo_seg,
            &ExportOptions {
                format: "yolo-seg".to_string(),
                polygon_policy: None,
                include_images: false,
            },
        )
        .unwrap();
        let seg_text =
            fs::read_to_string(yolo_seg.join("labels").join("val").join("b.txt")).unwrap();
        assert_eq!(
            yolo::parse_yolo_polygon_line(seg_text.trim(), 120, 90)
                .unwrap()
                .polygon
                .len(),
            3
        );

        let voc_root = root.join("voc");
        export_snapshot(
            &snapshot,
            &source_root,
            &voc_root,
            &ExportOptions {
                format: "voc-detect".to_string(),
                polygon_policy: Some("bbox".to_string()),
                include_images: true,
            },
        )
        .unwrap();
        let xml = fs::read_to_string(voc_root.join("Annotations").join("a.xml")).unwrap();
        assert_eq!(
            voc::parse_voc_annotations(&xml, &snapshot.classes)
                .unwrap()
                .len(),
            1
        );

        let coco_root = root.join("coco");
        let mut coco_snapshot = snapshot.clone();
        coco_snapshot.images[0].objects[0].attributes.insert(
            "coco.rawAnnotation".to_string(),
            serde_json::json!({
                "image_id": 999,
                "category_id": 999,
                "bbox": [0, 0, 1, 1],
                "source": "retained"
            }),
        );
        export_snapshot(
            &coco_snapshot,
            &source_root,
            &coco_root,
            &ExportOptions {
                format: "coco".to_string(),
                polygon_policy: None,
                include_images: true,
            },
        )
        .unwrap();
        let coco_dataset =
            coco::inspect_dataset(&coco_root, &coco_root.join("annotations.json")).unwrap();
        assert_eq!(coco_dataset.images.len(), 2);
        assert_eq!(coco_dataset.categories.len(), 2);
        assert_eq!(coco_dataset.images[1].objects[0].object_type, "polygon");
        let coco_json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(coco_root.join("annotations.json")).unwrap())
                .unwrap();
        let first_annotation = &coco_json["annotations"][0];
        assert_eq!(first_annotation["image_id"], serde_json::json!(1));
        assert_eq!(first_annotation["category_id"], serde_json::json!(1));
        assert_eq!(
            first_annotation["bbox"],
            serde_json::json!([10.0, 20.0, 30.0, 40.0])
        );
        assert_eq!(first_annotation["source"], serde_json::json!("retained"));

        let labelme_root = root.join("labelme");
        export_snapshot(
            &snapshot,
            &source_root,
            &labelme_root,
            &ExportOptions {
                format: "labelme".to_string(),
                polygon_policy: None,
                include_images: true,
            },
        )
        .unwrap();
        let labelme_data = fs::read_to_string(labelme_root.join("images").join("b.json")).unwrap();
        let parsed = labelme::parse_labelme(&labelme_data, &snapshot.classes).unwrap();
        assert_eq!(parsed.objects[0].object_type, "polygon");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yolo_detection_requires_explicit_polygon_loss_policy() {
        let (root, snapshot) = fixture();
        let error = export_snapshot(
            &snapshot,
            &root.join("source"),
            &root.join("blocked"),
            &ExportOptions {
                format: "yolo-detect".to_string(),
                polygon_policy: None,
                include_images: false,
            },
        )
        .unwrap_err();

        assert!(error.contains("polygonPolicy"));
        assert!(!root.join("blocked").exists());
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("image-annotation-{name}-{unique}"))
    }
}
