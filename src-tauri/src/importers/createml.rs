use crate::domain::{AnnotationObject, BBox};
use crate::importers::labelme::ExternalAnnotations;
use serde_json::{json, Value};

pub fn parse_createml(data: &str, image_name: &str, classes: &[String]) -> Result<ExternalAnnotations, String> {
    let root: Value = serde_json::from_str(data).map_err(|err| format!("parse CreateML JSON: {err}"))?;
    let records = root.as_array().ok_or("CreateML root must be an array")?;
    let record = records.iter().find(|record| record.get("image").and_then(Value::as_str) == Some(image_name))
        .ok_or_else(|| format!("CreateML has no record for image '{image_name}'"))?;
    let verified = record.get("verified").and_then(Value::as_bool).unwrap_or(false);
    let annotations = record.get("annotations").and_then(Value::as_array).ok_or("CreateML annotations must be an array")?;
    let mut objects = Vec::with_capacity(annotations.len());
    for (index, item) in annotations.iter().enumerate() {
        let label = item.get("label").and_then(Value::as_str)
            .filter(|label| !label.trim().is_empty())
            .ok_or_else(|| format!("annotation {index} has no label"))?;
        let class_id = classes.iter().position(|name| name == label)
            .ok_or_else(|| format!("annotation {index} uses unknown project class '{label}'"))? as u32;
        let coordinates = item.get("coordinates").ok_or("annotation has no coordinates")?;
        let number = |key: &str| -> Result<f64, String> {
            let value = coordinates.get(key).and_then(Value::as_f64).ok_or_else(|| format!("annotation {index} has no numeric {key}"))?;
            if !value.is_finite() { return Err(format!("annotation {index} has non-finite {key}")); }
            Ok(value)
        };
        let center_x = number("x")?;
        let center_y = number("y")?;
        let width = number("width")?;
        let height = number("height")?;
        if width <= 0.0 || height <= 0.0 || center_x < width / 2.0 || center_y < height / 2.0 {
            return Err(format!("annotation {index} has invalid bounding box"));
        }
        objects.push(AnnotationObject::bbox(format!("createml-{index}"), class_id, label.to_string(), BBox {
            x: center_x - width / 2.0, y: center_y - height / 2.0, width, height,
        }));
    }
    Ok(ExternalAnnotations { objects, verified })
}

pub fn write_createml(image_name: &str, objects: &[AnnotationObject], verified: bool) -> Result<String, String> {
    merge_createml("[]", image_name, objects, verified)
}

pub fn merge_createml(existing_data: &str, image_name: &str, objects: &[AnnotationObject], verified: bool) -> Result<String, String> {
    let root: Value = serde_json::from_str(existing_data).map_err(|err| format!("parse existing CreateML: {err}"))?;
    let mut records = root.as_array().ok_or("existing CreateML root must be an array")?.clone();
    let annotations = objects.iter().map(|object| -> Result<Value, String> {
        if object.object_type != "bbox" { return Err(format!("CreateML cannot represent annotation '{}' of type '{}'", object.id, object.object_type)); }
        let bbox = object.bbox.as_ref().ok_or_else(|| format!("annotation '{}' has no bbox", object.id))?;
        for (key, value) in &object.attributes {
            if value.is_null() || value == "" || value == false
                || value.as_object().is_some_and(serde_json::Map::is_empty) { continue; }
            if !matches!(key.as_str(), "source" | "format" | "split") {
                return Err(format!("CreateML cannot represent annotation '{}' attribute '{key}'", object.id));
            }
        }
        Ok(json!({"label": object.label, "coordinates": {
            "x": bbox.x + bbox.width / 2.0, "y": bbox.y + bbox.height / 2.0,
            "width": bbox.width, "height": bbox.height,
        }}))
    }).collect::<Result<Vec<_>, _>>()?;
    let record = json!({"image": image_name, "verified": verified, "annotations": annotations});
    if let Some(index) = records.iter().position(|entry| entry.get("image").and_then(Value::as_str) == Some(image_name)) {
        records[index] = record;
    } else {
        records.push(record);
    }
    serde_json::to_string_pretty(&records)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn createml_round_trip_and_rejects_unsupported_geometry() {
        let source = r#"[{"image":"样本.png","verified":true,"annotations":[{"label":"缺陷","coordinates":{"x":30,"y":40,"width":20,"height":10}}]}]"#;
        let imported = parse_createml(source, "样本.png", &["缺陷".to_string()]).unwrap();
        assert!(imported.verified);
        assert_eq!(imported.objects[0].bbox.as_ref().unwrap().x, 20.0);
        let output = write_createml("样本.png", &imported.objects, imported.verified).unwrap();
        assert_eq!(parse_createml(&output, "样本.png", &["缺陷".to_string()]).unwrap().objects.len(), 1);
        let mut polygon = AnnotationObject::polygon("p".into(), 0, "缺陷".into(), vec![
            crate::domain::Point { x: 1.0, y: 1.0 }, crate::domain::Point { x: 2.0, y: 1.0 }, crate::domain::Point { x: 2.0, y: 2.0 },
        ]);
        polygon.attributes.clear();
        assert!(write_createml("样本.png", &[polygon], false).unwrap_err().contains("p"));
        let merged = merge_createml(r#"[{"image":"other.png","annotations":[]}]"#, "样本.png", &imported.objects, true).unwrap();
        let records: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(records.as_array().unwrap().len(), 2);
        assert_eq!(records[0]["image"], "other.png");
    }
}
