use crate::{
    domain::{AnnotationObject, BBox, Point},
    importers::adapter::{
        source_version, verify_source_version, write_replacing, SourceSyncResult,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct CocoDataset {
    pub images: Vec<CocoImageRecord>,
    pub categories: Vec<CocoCategoryRecord>,
    pub unsupported_annotations: Vec<UnsupportedCocoAnnotation>,
    pub annotation_count: u32,
    pub source_version: String,
}

#[derive(Debug, Clone)]
pub struct CocoImageRecord {
    pub external_id: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub objects: Vec<AnnotationObject>,
}

#[derive(Debug, Clone)]
pub struct CocoCategoryRecord {
    pub external_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCocoAnnotation {
    pub id: String,
    pub image_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CocoFile {
    #[serde(default)]
    info: Option<Value>,
    #[serde(default)]
    licenses: Option<Value>,
    images: Vec<CocoImage>,
    annotations: Vec<CocoAnnotation>,
    categories: Vec<CocoCategory>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CocoImage {
    id: Value,
    file_name: String,
    width: u32,
    height: u32,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CocoCategory {
    id: Value,
    name: String,
    #[serde(default)]
    supercategory: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CocoAnnotation {
    id: Value,
    image_id: Value,
    category_id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bbox: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    segmentation: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    area: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iscrowd: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

pub fn inspect_dataset(root: &Path, annotation_path: &Path) -> Result<CocoDataset, String> {
    let data = fs::read_to_string(annotation_path).map_err(|err| err.to_string())?;
    let document: CocoFile = serde_json::from_str(&data).map_err(|err| err.to_string())?;
    inspect_document(root, annotation_path, &document)
}

pub fn find_annotation_path(root: &Path) -> Result<PathBuf, String> {
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file()
            || !entry
                .path()
                .extension()
                .map(|value| value.to_string_lossy().eq_ignore_ascii_case("json"))
                .unwrap_or(false)
        {
            continue;
        }
        let Ok(data) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if value.get("images").and_then(Value::as_array).is_some()
            && value.get("annotations").and_then(Value::as_array).is_some()
            && value.get("categories").and_then(Value::as_array).is_some()
        {
            return Ok(entry.path().to_path_buf());
        }
    }
    Err(format!(
        "COCO annotation JSON not found under {}",
        root.display()
    ))
}

pub fn load_image_annotations(
    root: &Path,
    annotation_path: &Path,
    external_image_id: &str,
) -> Result<Vec<AnnotationObject>, String> {
    inspect_dataset(root, annotation_path)?
        .images
        .into_iter()
        .find(|image| image.external_id == external_image_id)
        .map(|image| image.objects)
        .ok_or_else(|| format!("COCO image id not found: {external_image_id}"))
}

fn inspect_document(
    root: &Path,
    annotation_path: &Path,
    document: &CocoFile,
) -> Result<CocoDataset, String> {
    let mut image_ids = BTreeSet::new();
    let mut image_paths = BTreeSet::new();
    let mut images_by_id = BTreeMap::new();
    for image in &document.images {
        let external_id = id_key(&image.id, "image id")?;
        if !image_ids.insert(external_id.clone()) {
            return Err(format!("duplicate COCO image id {external_id}"));
        }
        let normalized_path = image.file_name.replace('\\', "/");
        if !image_paths.insert(normalized_path.clone()) {
            return Err(format!("duplicate COCO image file_name {normalized_path}"));
        }
        let source_path = root.join(&image.file_name);
        if !source_path.is_file() {
            return Err(format!(
                "COCO image file does not exist: {}",
                source_path.display()
            ));
        }
        images_by_id.insert(external_id, image);
    }

    let mut category_ids = BTreeSet::new();
    let mut categories_by_id = BTreeMap::new();
    let mut categories = Vec::new();
    for category in &document.categories {
        let external_id = id_key(&category.id, "category id")?;
        if !category_ids.insert(external_id.clone()) {
            return Err(format!("duplicate COCO category id {external_id}"));
        }
        let class_id = categories.len() as u32;
        categories_by_id.insert(external_id.clone(), (class_id, category.name.clone()));
        categories.push(CocoCategoryRecord {
            external_id,
            label: category.name.clone(),
        });
    }

    let mut objects_by_image: BTreeMap<String, Vec<AnnotationObject>> = BTreeMap::new();
    let mut unsupported_annotations = Vec::new();
    let mut annotation_ids = BTreeSet::new();
    for annotation in &document.annotations {
        let annotation_id = id_key(&annotation.id, "annotation id")?;
        if !annotation_ids.insert(annotation_id.clone()) {
            return Err(format!("duplicate COCO annotation id {annotation_id}"));
        }
        let image_id = id_key(&annotation.image_id, "annotation image_id")?;
        if !images_by_id.contains_key(&image_id) {
            return Err(format!(
                "COCO annotation {annotation_id} references missing image id {image_id}"
            ));
        }
        let category_id = id_key(&annotation.category_id, "annotation category_id")?;
        let Some((class_id, label)) = categories_by_id.get(&category_id) else {
            return Err(format!(
                "COCO annotation {annotation_id} references missing category id {category_id}"
            ));
        };
        if annotation.extra.contains_key("keypoints") {
            unsupported_annotations.push(UnsupportedCocoAnnotation {
                id: annotation_id,
                image_id,
                reason: "keypoints".to_string(),
            });
            continue;
        }
        match &annotation.segmentation {
            Some(Value::Object(_)) => {
                unsupported_annotations.push(UnsupportedCocoAnnotation {
                    id: annotation_id,
                    image_id,
                    reason: "rle-segmentation".to_string(),
                });
            }
            Some(Value::Array(polygons)) if !polygons.is_empty() => {
                for (segment_index, polygon) in polygons.iter().enumerate() {
                    let points = parse_polygon(&annotation_id, segment_index, polygon)?;
                    let mut attributes = annotation_attributes(annotation);
                    attributes.insert("coco.segmentIndex".to_string(), Value::from(segment_index));
                    objects_by_image
                        .entry(image_id.clone())
                        .or_default()
                        .push(AnnotationObject {
                            id: format!("coco-{annotation_id}-{segment_index}"),
                            class_id: *class_id,
                            label: label.clone(),
                            object_type: "polygon".to_string(),
                            bbox: None,
                            polygon: Some(points),
                            attributes,
                        });
                }
            }
            Some(Value::Array(_)) | None | Some(Value::Null) => {
                let bbox = parse_bbox(&annotation_id, annotation.bbox.as_deref())?;
                objects_by_image
                    .entry(image_id.clone())
                    .or_default()
                    .push(AnnotationObject {
                        id: format!("coco-{annotation_id}"),
                        class_id: *class_id,
                        label: label.clone(),
                        object_type: "bbox".to_string(),
                        bbox: Some(bbox),
                        polygon: None,
                        attributes: annotation_attributes(annotation),
                    });
            }
            Some(_) => {
                return Err(format!(
                    "COCO annotation {annotation_id} segmentation must be polygon arrays or RLE"
                ));
            }
        }
    }

    let images = document
        .images
        .iter()
        .map(|image| {
            let external_id = id_key(&image.id, "image id")?;
            Ok(CocoImageRecord {
                objects: objects_by_image.remove(&external_id).unwrap_or_default(),
                external_id,
                file_name: image.file_name.clone(),
                width: image.width,
                height: image.height,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(CocoDataset {
        images,
        categories,
        unsupported_annotations,
        annotation_count: document.annotations.len() as u32,
        source_version: source_version(annotation_path),
    })
}

pub fn sync_dataset(
    root: &Path,
    annotation_path: &Path,
    objects_by_image: &BTreeMap<String, Vec<AnnotationObject>>,
    expected_version: Option<&str>,
) -> Result<SourceSyncResult, String> {
    verify_source_version(annotation_path, expected_version)?;
    let data = fs::read_to_string(annotation_path).map_err(|err| err.to_string())?;
    let mut document: CocoFile = serde_json::from_str(&data).map_err(|err| err.to_string())?;
    let _ = inspect_document(root, annotation_path, &document)?;

    let category_ids = document
        .categories
        .iter()
        .map(|category| Ok((category.name.clone(), category.id.clone())))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let image_ids = document
        .images
        .iter()
        .map(|image| Ok((id_key(&image.id, "image id")?, image.id.clone())))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut next_annotation_id = document
        .annotations
        .iter()
        .filter_map(|annotation| annotation.id.as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let mut emitted_images = BTreeSet::new();
    let mut annotations = Vec::new();
    for annotation in &document.annotations {
        let image_key = id_key(&annotation.image_id, "annotation image_id")?;
        if annotation_is_unsupported(annotation) {
            annotations.push(annotation.clone());
            continue;
        }
        if emitted_images.insert(image_key.clone()) {
            if let Some(objects) = objects_by_image.get(&image_key) {
                annotations.extend(objects_to_annotations(
                    objects,
                    image_ids
                        .get(&image_key)
                        .ok_or_else(|| format!("COCO image id not found: {image_key}"))?,
                    &category_ids,
                    &mut next_annotation_id,
                )?);
            }
        }
    }
    for (image_key, objects) in objects_by_image {
        if emitted_images.insert(image_key.clone()) {
            annotations.extend(objects_to_annotations(
                objects,
                image_ids
                    .get(image_key)
                    .ok_or_else(|| format!("COCO image id not found: {image_key}"))?,
                &category_ids,
                &mut next_annotation_id,
            )?);
        }
    }
    document.annotations = annotations;
    let output = serde_json::to_string_pretty(&document).map_err(|err| err.to_string())?;
    write_replacing(annotation_path, output.as_bytes())
}

fn objects_to_annotations(
    objects: &[AnnotationObject],
    image_id: &Value,
    category_ids: &BTreeMap<String, Value>,
    next_annotation_id: &mut u64,
) -> Result<Vec<CocoAnnotation>, String> {
    let mut groups: Vec<(String, Vec<&AnnotationObject>)> = Vec::new();
    for object in objects {
        let key = object
            .attributes
            .get("coco.annotationId")
            .map(value_key)
            .unwrap_or_else(|| format!("new:{}", object.id));
        if let Some((_, items)) = groups.iter_mut().find(|(group, _)| group == &key) {
            items.push(object);
        } else {
            groups.push((key, vec![object]));
        }
    }
    groups
        .into_iter()
        .map(|(_, objects)| {
            let first = objects[0];
            let id = first
                .attributes
                .get("coco.annotationId")
                .cloned()
                .unwrap_or_else(|| {
                    let value = Value::from(*next_annotation_id);
                    *next_annotation_id += 1;
                    value
                });
            let category_id = first
                .attributes
                .get("coco.categoryId")
                .cloned()
                .or_else(|| category_ids.get(&first.label).cloned())
                .ok_or_else(|| format!("COCO category not found for label {}", first.label))?;
            let mut raw = first
                .attributes
                .get("coco.rawAnnotation")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let iscrowd = first.attributes.get("coco.iscrowd").cloned();
            let area = objects
                .iter()
                .map(|object| object_area(object))
                .collect::<Option<Vec<_>>>()
                .map(|areas| areas.into_iter().sum());
            let all_polygons = objects.iter().all(|object| object.polygon.is_some());
            let (bbox, segmentation) = if all_polygons {
                let polygons = objects
                    .iter()
                    .map(|object| {
                        Value::Array(
                            object
                                .polygon
                                .as_ref()
                                .expect("polygon checked")
                                .iter()
                                .flat_map(|point| [Value::from(point.x), Value::from(point.y)])
                                .collect(),
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    Some(bounds_for_objects(&objects)?),
                    Some(Value::Array(polygons)),
                )
            } else if objects.len() == 1 {
                let bbox = first
                    .bbox
                    .as_ref()
                    .ok_or_else(|| format!("COCO object {} has no supported geometry", first.id))?;
                (Some(vec![bbox.x, bbox.y, bbox.width, bbox.height]), None)
            } else {
                return Err(format!(
                    "COCO annotation group {} mixes incompatible geometries",
                    value_key(&id)
                ));
            };
            raw.remove("keypoints");
            Ok(CocoAnnotation {
                id,
                image_id: image_id.clone(),
                category_id,
                bbox,
                segmentation,
                area,
                iscrowd,
                extra: raw,
            })
        })
        .collect()
}

fn annotation_attributes(annotation: &CocoAnnotation) -> BTreeMap<String, Value> {
    let mut attributes = BTreeMap::new();
    attributes.insert("coco.annotationId".to_string(), annotation.id.clone());
    attributes.insert("coco.imageId".to_string(), annotation.image_id.clone());
    attributes.insert(
        "coco.categoryId".to_string(),
        annotation.category_id.clone(),
    );
    if let Some(value) = &annotation.iscrowd {
        attributes.insert("coco.iscrowd".to_string(), value.clone());
    }
    if let Some(value) = annotation.area {
        attributes.insert("coco.area".to_string(), Value::from(value));
    }
    if let Some(value) = &annotation.bbox {
        attributes.insert(
            "coco.sourceBbox".to_string(),
            Value::Array(value.iter().copied().map(Value::from).collect()),
        );
    }
    attributes.insert(
        "coco.rawAnnotation".to_string(),
        Value::Object(annotation.extra.clone()),
    );
    for (key, value) in &annotation.extra {
        attributes.insert(format!("coco.{key}"), value.clone());
    }
    attributes
}

fn parse_bbox(annotation_id: &str, bbox: Option<&[f64]>) -> Result<BBox, String> {
    let bbox = bbox.ok_or_else(|| format!("COCO annotation {annotation_id} has no bbox"))?;
    if bbox.len() != 4 || bbox.iter().any(|value| !value.is_finite()) {
        return Err(format!(
            "COCO annotation {annotation_id} bbox must contain 4 finite numbers"
        ));
    }
    if bbox[2] < 0.0 || bbox[3] < 0.0 {
        return Err(format!(
            "COCO annotation {annotation_id} bbox width and height must be non-negative"
        ));
    }
    Ok(BBox {
        x: bbox[0],
        y: bbox[1],
        width: bbox[2],
        height: bbox[3],
    })
}

fn parse_polygon(
    annotation_id: &str,
    segment_index: usize,
    value: &Value,
) -> Result<Vec<Point>, String> {
    let Some(values) = value.as_array() else {
        return Err(format!(
            "COCO annotation {annotation_id} polygon {segment_index} must be an array"
        ));
    };
    if values.len() < 6 || values.len() % 2 != 0 {
        return Err(format!(
            "COCO annotation {annotation_id} polygon {segment_index} must contain at least 3 coordinate pairs"
        ));
    }
    values
        .chunks_exact(2)
        .map(|pair| {
            let x = pair[0].as_f64().ok_or_else(|| {
                format!("COCO annotation {annotation_id} polygon {segment_index} has invalid x")
            })?;
            let y = pair[1].as_f64().ok_or_else(|| {
                format!("COCO annotation {annotation_id} polygon {segment_index} has invalid y")
            })?;
            if !x.is_finite() || !y.is_finite() {
                return Err(format!(
                    "COCO annotation {annotation_id} polygon {segment_index} has non-finite coordinates"
                ));
            }
            Ok(Point { x, y })
        })
        .collect()
}

fn annotation_is_unsupported(annotation: &CocoAnnotation) -> bool {
    annotation.extra.contains_key("keypoints")
        || matches!(annotation.segmentation, Some(Value::Object(_)))
}

fn bounds_for_objects(objects: &[&AnnotationObject]) -> Result<Vec<f64>, String> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for object in objects {
        let polygon = object
            .polygon
            .as_ref()
            .ok_or_else(|| format!("COCO polygon object {} has no points", object.id))?;
        for point in polygon {
            min_x = min_x.min(point.x);
            min_y = min_y.min(point.y);
            max_x = max_x.max(point.x);
            max_y = max_y.max(point.y);
        }
    }
    if !min_x.is_finite() {
        return Err("COCO polygon group has no points".to_string());
    }
    Ok(vec![min_x, min_y, max_x - min_x, max_y - min_y])
}

fn object_area(object: &AnnotationObject) -> Option<f64> {
    if let Some(bbox) = &object.bbox {
        return Some(bbox.width * bbox.height);
    }
    let polygon = object.polygon.as_ref()?;
    if polygon.len() < 3 {
        return None;
    }
    let mut area = 0.0;
    for index in 0..polygon.len() {
        let current = &polygon[index];
        let next = &polygon[(index + 1) % polygon.len()];
        area += current.x * next.y - next.x * current.y;
    }
    Some(area.abs() / 2.0)
}

fn id_key(value: &Value, field: &str) -> Result<String, String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(format!("COCO {field} must be a string or number")),
    }
}

fn value_key(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AnnotationObject, BBox};
    use serde_json::{json, Value};
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn fixture() -> Value {
        json!({
            "info": {"description": "fixture", "version": "1.0"},
            "licenses": [{"id": 1, "name": "fixture-license"}],
            "images": [
                {"id": 11, "file_name": "images/a.png", "width": 100, "height": 80, "camera": "left"},
                {"id": 12, "file_name": "images/b.png", "width": 120, "height": 90}
            ],
            "categories": [
                {"id": 3, "name": "defect", "supercategory": "quality"},
                {"id": 7, "name": "scratch", "supercategory": "quality"}
            ],
            "annotations": [
                {
                    "id": 101,
                    "image_id": 11,
                    "category_id": 3,
                    "bbox": [10.0, 20.0, 30.0, 40.0],
                    "area": 1200.0,
                    "iscrowd": 0,
                    "score": 0.91
                },
                {
                    "id": 102,
                    "image_id": 12,
                    "category_id": 7,
                    "segmentation": [[5.0, 5.0, 50.0, 5.0, 45.0, 40.0]],
                    "bbox": [5.0, 5.0, 45.0, 35.0],
                    "area": 900.0,
                    "iscrowd": 0
                },
                {
                    "id": 103,
                    "image_id": 12,
                    "category_id": 7,
                    "segmentation": {"counts": "abc", "size": [90, 120]},
                    "bbox": [0.0, 0.0, 120.0, 90.0],
                    "area": 10800.0,
                    "iscrowd": 1
                }
            ],
            "customTopLevel": {"keep": true}
        })
    }

    #[test]
    fn coco_imports_bbox_polygon_ids_and_metadata() {
        let root = temp_root("coco-import");
        fs::create_dir_all(root.join("images")).unwrap();
        image::RgbaImage::new(100, 80)
            .save(root.join("images").join("a.png"))
            .unwrap();
        image::RgbaImage::new(120, 90)
            .save(root.join("images").join("b.png"))
            .unwrap();
        let annotation_path = root.join("annotations.json");
        fs::write(
            &annotation_path,
            serde_json::to_string_pretty(&fixture()).unwrap(),
        )
        .unwrap();

        let dataset = inspect_dataset(&root, &annotation_path).unwrap();

        assert_eq!(dataset.images.len(), 2);
        assert_eq!(dataset.categories.len(), 2);
        assert_eq!(dataset.images[0].external_id, "11");
        assert_eq!(dataset.images[0].objects[0].object_type, "bbox");
        assert_eq!(
            dataset.images[0].objects[0].attributes["coco.annotationId"],
            json!(101)
        );
        assert_eq!(
            dataset.images[0].objects[0].attributes["coco.score"],
            json!(0.91)
        );
        assert_eq!(dataset.images[1].objects[0].object_type, "polygon");
        assert_eq!(dataset.unsupported_annotations.len(), 1);
        assert_eq!(
            dataset.unsupported_annotations[0].reason,
            "rle-segmentation"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn coco_rejects_duplicate_image_ids_missing_files_and_malformed_polygons() {
        let root = temp_root("coco-invalid");
        fs::create_dir_all(root.join("images")).unwrap();
        image::RgbaImage::new(100, 80)
            .save(root.join("images").join("a.png"))
            .unwrap();
        image::RgbaImage::new(120, 90)
            .save(root.join("images").join("b.png"))
            .unwrap();
        let annotation_path = root.join("annotations.json");

        let mut duplicate = fixture();
        duplicate["images"][1]["id"] = json!(11);
        fs::write(&annotation_path, duplicate.to_string()).unwrap();
        assert!(inspect_dataset(&root, &annotation_path)
            .unwrap_err()
            .contains("duplicate COCO image id 11"));

        let mut missing = fixture();
        missing["images"][1]["file_name"] = json!("images/missing.png");
        fs::write(&annotation_path, missing.to_string()).unwrap();
        assert!(inspect_dataset(&root, &annotation_path)
            .unwrap_err()
            .contains("COCO image file does not exist"));

        let mut malformed = fixture();
        malformed["annotations"][1]["segmentation"] = json!([[1.0, 2.0, 3.0]]);
        fs::write(&annotation_path, malformed.to_string()).unwrap();
        assert!(inspect_dataset(&root, &annotation_path)
            .unwrap_err()
            .contains("annotation 102 polygon 0"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn coco_sync_preserves_unedited_images_unsupported_records_and_top_level_metadata() {
        let root = temp_root("coco-sync");
        fs::create_dir_all(root.join("images")).unwrap();
        image::RgbaImage::new(100, 80)
            .save(root.join("images").join("a.png"))
            .unwrap();
        image::RgbaImage::new(120, 90)
            .save(root.join("images").join("b.png"))
            .unwrap();
        let annotation_path = root.join("annotations.json");
        fs::write(
            &annotation_path,
            serde_json::to_string_pretty(&fixture()).unwrap(),
        )
        .unwrap();
        let dataset = inspect_dataset(&root, &annotation_path).unwrap();
        let mut edited = BTreeMap::new();
        let mut edited_object = AnnotationObject::bbox(
            "edited".to_string(),
            0,
            "defect".to_string(),
            BBox {
                x: 25.0,
                y: 20.0,
                width: 35.0,
                height: 40.0,
            },
        );
        edited_object.attributes = dataset.images[0].objects[0].attributes.clone();
        edited.insert("11".to_string(), vec![edited_object]);
        edited.insert("12".to_string(), dataset.images[1].objects.clone());

        let result = sync_dataset(
            &root,
            &annotation_path,
            &edited,
            Some(&dataset.source_version),
        )
        .unwrap();
        let output: Value =
            serde_json::from_str(&fs::read_to_string(&result.path).unwrap()).unwrap();

        assert_eq!(output["info"]["description"], "fixture");
        assert_eq!(output["licenses"][0]["name"], "fixture-license");
        assert_eq!(output["customTopLevel"]["keep"], true);
        assert_eq!(output["images"][0]["camera"], "left");
        assert_eq!(output["annotations"][0]["bbox"][0], 25.0);
        assert_eq!(output["annotations"][0]["bbox"][2], 35.0);
        assert_eq!(output["annotations"][0]["area"], 1400.0);
        assert_eq!(output["annotations"][1]["segmentation"][0][0], 5.0);
        assert_eq!(output["annotations"][2]["segmentation"]["counts"], "abc");
        assert_eq!(output["annotations"][0]["score"], 0.91);
        assert!(!root.join("annotations.json.tmp").exists());

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
