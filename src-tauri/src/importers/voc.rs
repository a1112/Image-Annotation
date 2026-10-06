use crate::domain::{AnnotationObject, BBox};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename = "annotation")]
struct VocAnnotation {
    #[serde(default)]
    folder: Option<String>,
    filename: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    source: Option<VocSource>,
    size: VocSize,
    #[serde(default)]
    segmented: Option<u8>,
    #[serde(rename = "object", default)]
    objects: Vec<VocObject>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct VocSource {
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct VocSize {
    width: u32,
    height: u32,
    #[serde(default)]
    depth: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct VocObject {
    name: String,
    #[serde(default)]
    pose: Option<String>,
    #[serde(default)]
    truncated: Option<u8>,
    #[serde(default)]
    difficult: Option<u8>,
    #[serde(default)]
    confidence: Option<f64>,
    bndbox: VocBndBox,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct VocBndBox {
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
}

pub fn parse_voc_annotations(
    xml: &str,
    labels: &[String],
) -> Result<Vec<AnnotationObject>, String> {
    let annotation: VocAnnotation = quick_xml::de::from_str(xml).map_err(|err| err.to_string())?;
    Ok(annotation
        .objects
        .into_iter()
        .enumerate()
        .map(|(index, object)| {
            let class_id = labels
                .iter()
                .position(|label| label == &object.name)
                .unwrap_or(index) as u32;
            let mut attributes = BTreeMap::new();
            attributes.insert("format".to_string(), json!("pascal-voc"));
            if let Some(difficult) = object.difficult {
                attributes.insert("difficult".to_string(), json!(difficult == 1));
            }
            if let Some(truncated) = object.truncated {
                attributes.insert("truncated".to_string(), json!(truncated == 1));
            }
            if let Some(confidence) = object.confidence {
                attributes.insert("confidence".to_string(), json!(confidence));
            }
            if let Some(pose) = object.pose {
                attributes.insert("pose".to_string(), json!(pose));
            }
            AnnotationObject {
                id: format!("voc-{index}"),
                class_id,
                label: object.name,
                object_type: "bbox".to_string(),
                bbox: Some(BBox {
                    x: object.bndbox.xmin,
                    y: object.bndbox.ymin,
                    width: (object.bndbox.xmax - object.bndbox.xmin).max(1.0),
                    height: (object.bndbox.ymax - object.bndbox.ymin).max(1.0),
                }),
                polygon: None,
                points: None,
                mask_data: None,
                attributes,
            }
        })
        .collect())
}

pub fn parse_voc_labels(xml: &str) -> Result<Vec<String>, String> {
    let annotation: VocAnnotation = quick_xml::de::from_str(xml).map_err(|err| err.to_string())?;
    let mut labels = BTreeSet::new();
    for object in annotation.objects {
        labels.insert(object.name);
    }
    Ok(labels.into_iter().collect())
}

pub fn annotations_to_voc_xml(
    image_path: &Path,
    width: u32,
    height: u32,
    objects: &[AnnotationObject],
) -> Result<String, String> {
    annotations_to_voc_xml_preserving(image_path, width, height, objects, None)
}

pub fn annotations_to_voc_xml_preserving(
    image_path: &Path,
    width: u32,
    height: u32,
    objects: &[AnnotationObject],
    existing_xml: Option<&str>,
) -> Result<String, String> {
    for object in objects {
        if object.object_type != "bbox" || object.bbox.is_none() {
            return Err(format!(
                "Pascal VOC cannot represent annotation '{}' of type '{}'",
                object.id, object.object_type
            ));
        }
    }
    let filename = image_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "image.jpg".to_string());
    let folder = image_path
        .parent()
        .and_then(|path| path.file_name())
        .map(|value| value.to_string_lossy().to_string());
    let mut annotation = VocAnnotation {
        folder,
        filename,
        path: Some(image_path.to_string_lossy().to_string()),
        source: Some(VocSource {
            database: Some("Image Annotation".to_string()),
            source_path: Some(image_path.to_string_lossy().to_string()),
        }),
        size: VocSize {
            width,
            height,
            depth: Some(3),
        },
        segmented: Some(0),
        objects: objects
            .iter()
            .filter_map(|object| {
                let bbox = object.bbox.as_ref()?;
                Some(VocObject {
                    name: object.label.clone(),
                    pose: Some(object.attributes.get("pose").and_then(|value| value.as_str())
                        .unwrap_or("Unspecified").to_string()),
                    truncated: Some(attribute_bool(object, "truncated") as u8),
                    difficult: Some(attribute_bool(object, "difficult") as u8),
                    confidence: object
                        .attributes
                        .get("confidence")
                        .and_then(|value| value.as_f64()),
                    bndbox: VocBndBox {
                        xmin: bbox.x.round(),
                        ymin: bbox.y.round(),
                        xmax: (bbox.x + bbox.width).round(),
                        ymax: (bbox.y + bbox.height).round(),
                    },
                })
            })
            .collect(),
    };
    if let Some(xml) = existing_xml {
        validate_supported_voc_fields(xml)?;
        let original: VocAnnotation = quick_xml::de::from_str(xml).map_err(|error| error.to_string())?;
        if original.size.width != width || original.size.height != height {
            return Err("existing VOC dimensions differ from the image; source labels were left unchanged".to_string());
        }
        annotation.folder = original.folder;
        annotation.filename = original.filename;
        annotation.path = original.path;
        annotation.source = original.source;
        annotation.size.depth = original.size.depth;
        annotation.segmented = original.segmented;
    }
    let body = quick_xml::se::to_string(&annotation).map_err(|err| err.to_string())?;
    Ok(format!("<?xml version='1.0' encoding='utf-8'?>\n{body}\n"))
}

fn validate_supported_voc_fields(xml: &str) -> Result<(), String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event().map_err(|error| error.to_string())? {
            Event::Start(event) | Event::Empty(event) => {
                let name = String::from_utf8_lossy(event.name().as_ref()).to_string();
                if !matches!(name.as_str(), "annotation" | "folder" | "filename" | "path"
                    | "source" | "database" | "source_path" | "size" | "width" | "height"
                    | "depth" | "segmented" | "object" | "name" | "pose" | "truncated"
                    | "difficult" | "confidence" | "bndbox" | "xmin" | "ymin" | "xmax" | "ymax")
                    || event.attributes().next().is_some() {
                    return Err(format!("VOC source contains unsupported '{name}' metadata"));
                }
            }
            Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {
                return Err("VOC source contains comments or extensions that must be preserved".to_string());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

fn attribute_bool(object: &AnnotationObject, key: &str) -> bool {
    object.attributes.get(key).and_then(|value| value.as_bool()).unwrap_or(false)
        || object.attributes.get("flags").and_then(|value| value.get(key))
            .and_then(|value| value.as_bool()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pascal_voc_bbox_objects() {
        let labels = vec!["毛刺".to_string()];
        let objects = parse_voc_annotations(
            r#"
            <annotation>
              <filename>a.jpg</filename>
              <size><width>1024</width><height>1024</height><depth>3</depth></size>
              <object>
                <name>毛刺</name>
                <difficult>0</difficult>
                <confidence>0.523366</confidence>
                <bndbox><xmin>911</xmin><ymin>748</ymin><xmax>957</xmax><ymax>779</ymax></bndbox>
              </object>
            </annotation>
            "#,
            &labels,
        )
        .unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].label, "毛刺");
        assert_eq!(objects[0].bbox.as_ref().unwrap().x, 911.0);
        assert_eq!(objects[0].bbox.as_ref().unwrap().width, 46.0);
        assert_eq!(objects[0].attributes["confidence"], json!(0.523366));
    }

    #[test]
    fn writes_pascal_voc_xml_for_labelimg_style_bboxes() {
        let objects = vec![AnnotationObject::bbox(
            "a".to_string(),
            0,
            "毛刺".to_string(),
            BBox {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
        )];

        let xml =
            annotations_to_voc_xml(Path::new("L:/data/out/a.jpg"), 1024, 1024, &objects).unwrap();

        assert!(xml.contains("<filename>a.jpg</filename>"));
        assert!(xml.contains("<name>毛刺</name>"));
        assert!(xml.contains("<xmin>10</xmin>"));
        assert!(xml.contains("<xmax>40</xmax>"));
    }

    #[test]
    fn preserves_existing_voc_metadata_and_rejects_unknown_extensions() {
        let original = "<annotation><folder>original</folder><filename>a.jpg</filename><source><database>labelImg</database></source><size><width>100</width><height>80</height><depth>1</depth></size><segmented>1</segmented><object><name>person</name><pose>Left</pose><truncated>1</truncated><difficult>0</difficult><bndbox><xmin>1</xmin><ymin>2</ymin><xmax>11</xmax><ymax>12</ymax></bndbox></object></annotation>";
        let mut objects = parse_voc_annotations(original, &["person".to_string()]).unwrap();
        objects[0].attributes.insert("flags".into(), json!({"difficult": true}));
        let written = annotations_to_voc_xml_preserving(Path::new("a.jpg"), 100, 80, &objects, Some(original)).unwrap();
        assert!(written.contains("<database>labelImg</database>"));
        assert!(written.contains("<folder>original</folder>"));
        assert!(written.contains("<pose>Left</pose>"));
        assert!(written.contains("<difficult>1</difficult>"));
        assert!(written.contains("<segmented>1</segmented>"));
        assert!(annotations_to_voc_xml_preserving(Path::new("a.jpg"), 100, 80, &objects,
            Some(&original.replace("</annotation>", "<custom>retain</custom></annotation>"))).unwrap_err()
            .contains("unsupported"));
    }

    #[test]
    fn rejects_shapes_that_pascal_voc_cannot_represent() {
        let objects = vec![AnnotationObject::polygon(
            "poly-1".to_string(),
            0,
            "region".to_string(),
            vec![
                crate::domain::Point { x: 1.0, y: 1.0 },
                crate::domain::Point { x: 5.0, y: 1.0 },
                crate::domain::Point { x: 1.0, y: 5.0 },
            ],
        )];
        let error = annotations_to_voc_xml(Path::new("a.jpg"), 16, 16, &objects).unwrap_err();
        assert!(error.contains("poly-1"), "{error}");
    }
}
