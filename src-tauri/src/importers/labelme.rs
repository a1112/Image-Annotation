use crate::domain::{AnnotationObject, BBox, Point};
use crate::importers::adapter::{
    source_version, verify_source_version, write_replacing, SourceSyncResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LabelMeFile {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    flags: Map<String, Value>,
    #[serde(default)]
    shapes: Vec<LabelMeShape>,
    image_path: String,
    #[serde(default)]
    image_data: Option<Value>,
    image_height: u32,
    image_width: u32,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LabelMeShape {
    label: String,
    points: Vec<[f64; 2]>,
    #[serde(default)]
    group_id: Option<Value>,
    shape_type: String,
    #[serde(default)]
    flags: Map<String, Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedLabelMeShape {
    pub index: usize,
    pub shape_type: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ParsedLabelMe {
    pub image_path: String,
    pub image_width: u32,
    pub image_height: u32,
    pub objects: Vec<AnnotationObject>,
    pub unsupported_shapes: Vec<UnsupportedLabelMeShape>,
    template: LabelMeFile,
}

#[derive(Debug, Clone)]
pub struct LoadedLabelMe {
    pub objects: Vec<AnnotationObject>,
    pub unsupported_shapes: Vec<UnsupportedLabelMeShape>,
    pub source_version: String,
}

pub fn annotation_path(_root: &Path, image_path: &Path) -> PathBuf {
    image_path.with_extension("json")
}

pub fn current_source_version(root: &Path, image_path: &Path) -> String {
    source_version(&annotation_path(root, image_path))
}

pub fn labels_from_json(data: &str) -> Result<Vec<String>, String> {
    let parsed: LabelMeFile = serde_json::from_str(data).map_err(|err| err.to_string())?;
    let mut labels = parsed
        .shapes
        .into_iter()
        .map(|shape| shape.label)
        .filter(|label| !label.trim().is_empty())
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    Ok(labels)
}

pub fn load_annotations(
    root: &Path,
    image_path: &Path,
    labels: &[String],
) -> Result<LoadedLabelMe, String> {
    let path = annotation_path(root, image_path);
    if !path.exists() {
        return Ok(LoadedLabelMe {
            objects: Vec::new(),
            unsupported_shapes: Vec::new(),
            source_version: String::new(),
        });
    }
    let data = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let parsed = parse_labelme(&data, labels)?;
    Ok(LoadedLabelMe {
        objects: parsed.objects,
        unsupported_shapes: parsed.unsupported_shapes,
        source_version: source_version(&path),
    })
}

pub fn sync_annotations(
    root: &Path,
    image_path: &Path,
    objects: &[AnnotationObject],
    expected_version: Option<&str>,
) -> Result<SourceSyncResult, String> {
    let path = annotation_path(root, image_path);
    verify_source_version(&path, expected_version)?;
    let parsed = if path.exists() {
        let data = fs::read_to_string(&path).map_err(|err| err.to_string())?;
        parse_labelme(&data, &[])?
    } else {
        let (image_width, image_height) =
            image::image_dimensions(image_path).map_err(|err| err.to_string())?;
        let image_name = image_path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .ok_or_else(|| format!("image file name not found: {}", image_path.display()))?;
        ParsedLabelMe {
            image_path: image_name.clone(),
            image_width,
            image_height,
            objects: Vec::new(),
            unsupported_shapes: Vec::new(),
            template: LabelMeFile {
                version: Some("5.0.1".to_string()),
                flags: Map::new(),
                shapes: Vec::new(),
                image_path: image_name,
                image_data: None,
                image_height,
                image_width,
                extra: Map::new(),
            },
        }
    };
    let output = annotations_to_labelme_json(&parsed, objects)?;
    write_replacing(&path, output.as_bytes())
}

pub fn parse_labelme(data: &str, labels: &[String]) -> Result<ParsedLabelMe, String> {
    let template: LabelMeFile = serde_json::from_str(data).map_err(|err| err.to_string())?;
    let mut objects = Vec::new();
    let mut unsupported_shapes = Vec::new();
    for (index, shape) in template.shapes.iter().enumerate() {
        let class_id = labels
            .iter()
            .position(|label| label == &shape.label)
            .unwrap_or(index) as u32;
        let mut attributes = BTreeMap::new();
        if let Some(group_id) = &shape.group_id {
            attributes.insert("labelme.groupId".to_string(), group_id.clone());
        }
        attributes.insert(
            "labelme.shapeFlags".to_string(),
            Value::Object(shape.flags.clone()),
        );
        attributes.insert(
            "labelme.rawShape".to_string(),
            Value::Object(shape.extra.clone()),
        );

        let object = match shape.shape_type.as_str() {
            "rectangle" if shape.points.len() >= 2 => {
                let first = shape.points[0];
                let second = shape.points[1];
                Some(AnnotationObject {
                    id: format!("labelme-{index}"),
                    class_id,
                    label: shape.label.clone(),
                    object_type: "bbox".to_string(),
                    bbox: Some(BBox {
                        x: first[0].min(second[0]),
                        y: first[1].min(second[1]),
                        width: (first[0] - second[0]).abs().max(1.0),
                        height: (first[1] - second[1]).abs().max(1.0),
                    }),
                    polygon: None,
                    attributes,
                })
            }
            "polygon" if shape.points.len() >= 3 => Some(AnnotationObject {
                id: format!("labelme-{index}"),
                class_id,
                label: shape.label.clone(),
                object_type: "polygon".to_string(),
                bbox: None,
                polygon: Some(
                    shape
                        .points
                        .iter()
                        .map(|point| Point {
                            x: point[0],
                            y: point[1],
                        })
                        .collect(),
                ),
                attributes,
            }),
            _ => {
                unsupported_shapes.push(UnsupportedLabelMeShape {
                    index,
                    shape_type: shape.shape_type.clone(),
                    label: shape.label.clone(),
                });
                None
            }
        };
        if let Some(object) = object {
            objects.push(object);
        }
    }
    Ok(ParsedLabelMe {
        image_path: template.image_path.clone(),
        image_width: template.image_width,
        image_height: template.image_height,
        objects,
        unsupported_shapes,
        template,
    })
}

pub fn annotations_to_labelme_json(
    parsed: &ParsedLabelMe,
    objects: &[AnnotationObject],
) -> Result<String, String> {
    let mut output = parsed.template.clone();
    let unsupported = parsed
        .unsupported_shapes
        .iter()
        .filter_map(|unsupported| parsed.template.shapes.get(unsupported.index).cloned())
        .collect::<Vec<_>>();
    let mut shapes = objects
        .iter()
        .map(object_to_shape)
        .collect::<Result<Vec<_>, _>>()?;
    shapes.extend(unsupported);
    output.shapes = shapes;
    serde_json::to_string_pretty(&output).map_err(|err| err.to_string())
}

fn object_to_shape(object: &AnnotationObject) -> Result<LabelMeShape, String> {
    let group_id = object.attributes.get("labelme.groupId").cloned();
    let flags = object
        .attributes
        .get("labelme.shapeFlags")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let extra = object
        .attributes
        .get("labelme.rawShape")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let (shape_type, points) = if let Some(bbox) = &object.bbox {
        (
            "rectangle".to_string(),
            vec![
                [bbox.x, bbox.y],
                [bbox.x + bbox.width, bbox.y + bbox.height],
            ],
        )
    } else if let Some(polygon) = &object.polygon {
        if polygon.len() < 3 {
            return Err(format!(
                "LabelMe polygon object {} must contain at least 3 points",
                object.id
            ));
        }
        (
            "polygon".to_string(),
            polygon.iter().map(|point| [point.x, point.y]).collect(),
        )
    } else {
        return Err(format!("unsupported empty annotation object {}", object.id));
    };
    Ok(LabelMeShape {
        label: object.label.clone(),
        points,
        group_id,
        shape_type,
        flags,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    const FIXTURE: &str = r#"
    {
      "version": "5.5.0",
      "flags": {"reviewed": true},
      "customFileField": "keep-me",
      "shapes": [
        {
          "label": "defect",
          "points": [[10, 20], [40, 60]],
          "group_id": 7,
          "shape_type": "rectangle",
          "flags": {},
          "customShapeField": 12
        },
        {
          "label": "scratch",
          "points": [[5, 5], [30, 5], [20, 40]],
          "group_id": null,
          "shape_type": "polygon",
          "flags": {"hard": true}
        },
        {
          "label": "ignored",
          "points": [[50, 50], [60, 50]],
          "group_id": null,
          "shape_type": "circle",
          "flags": {}
        }
      ],
      "imagePath": "a.png",
      "imageData": null,
      "imageHeight": 100,
      "imageWidth": 100
    }
    "#;

    #[test]
    fn labelme_rectangles_polygons_and_metadata_round_trip() {
        let parsed =
            parse_labelme(FIXTURE, &["defect".to_string(), "scratch".to_string()]).unwrap();

        assert_eq!(parsed.image_path, "a.png");
        assert_eq!(parsed.objects.len(), 2);
        assert!(parsed.objects[0].bbox.is_some());
        assert!(parsed.objects[1].polygon.is_some());
        assert_eq!(parsed.objects[0].attributes["labelme.groupId"], json!(7));
        assert_eq!(
            parsed.objects[1].attributes["labelme.shapeFlags"],
            json!({"hard": true})
        );
        assert_eq!(parsed.unsupported_shapes.len(), 1);
        assert_eq!(parsed.unsupported_shapes[0].shape_type, "circle");
        assert_eq!(parsed.unsupported_shapes[0].index, 2);

        let output = annotations_to_labelme_json(&parsed, &parsed.objects).unwrap();
        let reparsed =
            parse_labelme(&output, &["defect".to_string(), "scratch".to_string()]).unwrap();
        let output_value: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(reparsed.objects.len(), 2);
        assert_eq!(output_value["flags"], json!({"reviewed": true}));
        assert_eq!(output_value["customFileField"], json!("keep-me"));
        assert_eq!(output_value["shapes"][0]["group_id"], json!(7));
        assert_eq!(output_value["shapes"][0]["customShapeField"], json!(12));
        assert_eq!(output_value["shapes"][1]["flags"], json!({"hard": true}));
    }

    #[test]
    fn labelme_linked_sidecar_loads_and_writes_atomically() {
        let root = temp_root("labelme-linked");
        fs::create_dir_all(&root).unwrap();
        let image_path = root.join("a.png");
        let annotation_path = root.join("a.json");
        image::RgbaImage::new(100, 100).save(&image_path).unwrap();
        fs::write(&annotation_path, FIXTURE).unwrap();
        let labels = vec!["defect".to_string(), "scratch".to_string()];

        let loaded = load_annotations(&root, &image_path, &labels).unwrap();
        let result = sync_annotations(
            &root,
            &image_path,
            &loaded.objects,
            Some(&loaded.source_version),
        )
        .unwrap();
        let reparsed = parse_labelme(&fs::read_to_string(&result.path).unwrap(), &labels).unwrap();

        assert_eq!(result.path, annotation_path);
        assert_eq!(reparsed.objects.len(), 2);
        assert_eq!(reparsed.unsupported_shapes.len(), 1);
        assert!(!root.join("a.json.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn labelme_creates_sidecar_for_previously_unannotated_image() {
        let root = temp_root("labelme-new-sidecar");
        let image_path = root.join("nested").join("new.png");
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        image::RgbaImage::new(320, 240).save(&image_path).unwrap();
        let object = AnnotationObject::bbox(
            "new-1".to_string(),
            0,
            "defect".to_string(),
            BBox {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
        );

        let result = sync_annotations(&root, &image_path, &[object], None).unwrap();
        let output: Value =
            serde_json::from_str(&fs::read_to_string(&result.path).unwrap()).unwrap();

        assert_eq!(result.path, image_path.with_extension("json"));
        assert_eq!(output["imagePath"], "new.png");
        assert_eq!(output["imageWidth"], 320);
        assert_eq!(output["imageHeight"], 240);
        assert_eq!(output["shapes"][0]["label"], "defect");

        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("image-annotation-{name}-{unique}"))
    }
}
