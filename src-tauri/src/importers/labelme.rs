use crate::domain::{AnnotationObject, BBox, Point};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAnnotations {
    pub objects: Vec<AnnotationObject>,
    pub verified: bool,
}

fn parse_points(value: &Value) -> Result<Vec<Point>, String> {
    value.as_array().ok_or("shape points must be an array")?.iter().map(|pair| {
        let coordinates = pair.as_array().ok_or("shape point must be a pair")?;
        if coordinates.len() != 2 { return Err("shape point must have two coordinates".into()); }
        let x = coordinates[0].as_f64().ok_or("point x must be a number")?;
        let y = coordinates[1].as_f64().ok_or("point y must be a number")?;
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
            return Err("shape point is outside the image coordinate space".into());
        }
        Ok(Point { x, y })
    }).collect()
}

pub fn parse_labelme(data: &str, classes: &[String]) -> Result<ExternalAnnotations, String> {
    let root: Value = serde_json::from_str(data).map_err(|err| format!("parse LabelMe JSON: {err}"))?;
    let shapes = root.get("shapes").and_then(Value::as_array)
        .ok_or("LabelMe shapes must be an array")?;
    let verified = root.pointer("/flags/verified").and_then(Value::as_bool).unwrap_or(false);
    let mut objects = Vec::with_capacity(shapes.len());
    for (index, shape) in shapes.iter().enumerate() {
        let label = shape.get("label").and_then(Value::as_str)
            .filter(|label| !label.trim().is_empty())
            .ok_or_else(|| format!("shape {index} has no label"))?.to_string();
        let class_id = classes.iter().position(|item| item == &label)
            .ok_or_else(|| format!("shape {index} uses unknown project class '{label}'"))? as u32;
        let shape_type = shape.get("shape_type").and_then(Value::as_str).unwrap_or("polygon");
        let points = parse_points(shape.get("points").ok_or("shape has no points")?)?;
        let id = shape.get("imageAnnotationId").and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty()).map(str::to_string)
            .unwrap_or_else(|| format!("labelme-{index}"));
        let mut object = match shape_type {
            "rectangle" => {
                if points.len() != 2 { return Err(format!("shape {index} rectangle needs two corners")); }
                let x = points[0].x.min(points[1].x);
                let y = points[0].y.min(points[1].y);
                let width = (points[0].x - points[1].x).abs();
                let height = (points[0].y - points[1].y).abs();
                AnnotationObject::bbox(id, class_id, label, BBox { x, y, width, height })
            }
            "polygon" => AnnotationObject::polygon(id, class_id, label, points),
            "oriented_rectangle" | "circle" | "line" | "linestrip" | "point" | "points" | "mask" => AnnotationObject {
                id, class_id, label, object_type: shape_type.to_string(),
                bbox: None, polygon: None, points: Some(points),
                mask_data: shape.get("mask").and_then(Value::as_str).map(str::to_string),
                attributes: Default::default(),
            },
            other => return Err(format!("shape {index} has unsupported type '{other}'")),
        };
        if let Some(value) = shape.get("group_id").filter(|value| !value.is_null()) {
            if !value.is_i64() { return Err(format!("shape {index} group_id must be integer")); }
            object.attributes.insert("groupId".to_string(), value.clone());
        }
        if let Some(description) = shape.get("description").and_then(Value::as_str) {
            object.attributes.insert("description".to_string(), json!(description));
        }
        if let Some(flags) = shape.get("flags").and_then(Value::as_object) {
            let difficult = flags.get("difficult").and_then(Value::as_bool).unwrap_or(false);
            if difficult { object.attributes.insert("difficult".to_string(), json!(true)); }
            object.attributes.insert("flags".to_string(), Value::Object(flags.clone()));
        }
        if let Some(extra_attributes) = shape.get("imageAnnotationAttributes").and_then(Value::as_object) {
            for (key, value) in extra_attributes {
                object.attributes.insert(key.clone(), value.clone());
            }
        }
        if let Some(other) = shape.as_object() {
            let extras = other.iter().filter(|(key, _)| !matches!(key.as_str(),
                "label" | "points" | "group_id" | "description" | "shape_type" | "flags" | "mask" | "imageAnnotationAttributes" | "imageAnnotationId"))
                .map(|(key, value)| (key.clone(), value.clone())).collect::<serde_json::Map<_, _>>();
            if !extras.is_empty() { object.attributes.insert("labelmeOtherData".to_string(), Value::Object(extras)); }
        }
        objects.push(object);
    }
    Ok(ExternalAnnotations { objects, verified })
}

pub fn write_labelme(image_path: &Path, width: u32, height: u32, objects: &[AnnotationObject], verified: bool) -> Result<String, String> {
    let shapes = objects.iter().map(|object| -> Result<Value, String> {
        let (shape_type, points) = match object.object_type.as_str() {
            "bbox" => {
                let bbox = object.bbox.as_ref().ok_or_else(|| format!("annotation '{}' has no bbox", object.id))?;
                ("rectangle", vec![Point { x: bbox.x, y: bbox.y }, Point { x: bbox.x + bbox.width, y: bbox.y + bbox.height }])
            }
            "polygon" => ("polygon", object.polygon.clone().ok_or_else(|| format!("annotation '{}' has no polygon", object.id))?),
            "oriented_rectangle" | "circle" | "line" | "linestrip" | "point" | "points" | "mask" =>
                (object.object_type.as_str(), object.points.clone().ok_or_else(|| format!("annotation '{}' has no points", object.id))?),
            other => return Err(format!("LabelMe cannot represent annotation '{}' of type '{other}'", object.id)),
        };
        let mut flags = object.attributes.get("flags").and_then(Value::as_object).cloned().unwrap_or_default();
        if object.attributes.get("difficult").and_then(Value::as_bool) == Some(true) { flags.insert("difficult".into(), json!(true)); }
        let point_pairs = points.into_iter().map(|point| json!([point.x, point.y])).collect::<Vec<_>>();
        let mut shape = object.attributes.get("labelmeOtherData").and_then(Value::as_object).cloned().unwrap_or_default();
        shape.insert("label".into(), json!(object.label));
        shape.insert("imageAnnotationId".into(), json!(object.id));
        shape.insert("points".into(), json!(point_pairs));
        shape.insert("shape_type".into(), json!(shape_type));
        shape.insert("group_id".into(), object.attributes.get("groupId").cloned().unwrap_or(Value::Null));
        shape.insert("description".into(), object.attributes.get("description").cloned().unwrap_or(json!("")));
        shape.insert("flags".into(), Value::Object(flags));
        shape.insert("mask".into(), object.mask_data.as_ref().map(|data| json!(data)).unwrap_or(Value::Null));
        let extra = object.attributes.iter()
            .filter(|(key, _)| !matches!(key.as_str(), "groupId" | "description" | "flags" | "difficult" | "labelmeOtherData"))
            .map(|(key, value)| (key.clone(), value.clone())).collect::<serde_json::Map<_, _>>();
        if !extra.is_empty() { shape.insert("imageAnnotationAttributes".into(), Value::Object(extra)); }
        Ok(Value::Object(shape))
    }).collect::<Result<Vec<_>, _>>()?;
    let filename = image_path.file_name().ok_or("image path has no filename")?.to_string_lossy();
    serde_json::to_string_pretty(&json!({
        "version": "5.7.0", "flags": { "verified": verified }, "shapes": shapes,
        "imagePath": filename, "imageData": null, "imageWidth": width, "imageHeight": height,
    })).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labelme_round_trip_retains_shapes_metadata_and_chinese_label() {
        let source = r#"{"version":"5.7.0","flags":{"verified":true},"imagePath":"样本.png","imageData":null,"imageWidth":100,"imageHeight":80,"shapes":[{"label":"毛刺","points":[[10,20],[30,40]],"group_id":7,"description":"边缘","shape_type":"rectangle","flags":{"difficult":true},"mask":null},{"label":"毛刺","points":[[1,2],[5,6]],"group_id":null,"description":"","shape_type":"mask","flags":{},"mask":"aGVsbG8="}]}"#;
        let imported = parse_labelme(source, &["毛刺".to_string()]).unwrap();
        assert!(imported.verified);
        assert_eq!(imported.objects.len(), 2);
        assert_eq!(imported.objects[0].attributes["groupId"], 7);
        assert_eq!(imported.objects[0].attributes["difficult"], true);
        let mut imported = imported;
        imported.objects[0].attributes.insert("lineColor".into(), json!("#ff0000"));
        let output = write_labelme(Path::new("样本.png"), 100, 80, &imported.objects, imported.verified).unwrap();
        let reloaded = parse_labelme(&output, &["毛刺".to_string()]).unwrap();
        assert_eq!(reloaded.objects[0].label, "毛刺");
        assert_eq!(reloaded.objects[0].id, imported.objects[0].id);
        assert_eq!(reloaded.objects[0].attributes["lineColor"], "#ff0000");
        assert_eq!(reloaded.objects[1].mask_data.as_deref(), Some("aGVsbG8="));
    }
}
