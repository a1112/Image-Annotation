use crate::domain::{AnnotationObject, BBox, Point};

#[derive(Debug, Clone)]
pub struct ParsedBbox {
    pub class_id: u32,
    pub bbox: BBox,
}

#[derive(Debug, Clone)]
pub struct ParsedPolygon {
    pub class_id: u32,
    pub polygon: Vec<Point>,
}

pub fn parse_yolo_bbox_line(
    line: &str,
    image_width: u32,
    image_height: u32,
) -> Result<ParsedBbox, String> {
    let values = parse_f64_values(line)?;
    if values.len() != 5 {
        return Err("YOLO bbox line must contain class and 4 numbers".to_string());
    }

    let class_id = parse_class_id(values[0])?;
    if values[1..].iter().any(|value| !(0.0..=1.0).contains(value))
        || values[3] <= 0.0 || values[4] <= 0.0
        || values[1] - values[3] / 2.0 < -1e-6
        || values[2] - values[4] / 2.0 < -1e-6
        || values[1] + values[3] / 2.0 > 1.0 + 1e-6
        || values[2] + values[4] / 2.0 > 1.0 + 1e-6 {
        return Err("YOLO bbox coordinates must describe a positive box inside the image".to_string());
    }
    let width = values[3] * image_width as f64;
    let height = values[4] * image_height as f64;
    let center_x = values[1] * image_width as f64;
    let center_y = values[2] * image_height as f64;

    Ok(ParsedBbox {
        class_id,
        bbox: BBox {
            x: round1(center_x - width / 2.0),
            y: round1(center_y - height / 2.0),
            width: round1(width),
            height: round1(height),
        },
    })
}

pub fn parse_yolo_polygon_line(
    line: &str,
    image_width: u32,
    image_height: u32,
) -> Result<ParsedPolygon, String> {
    let values = parse_f64_values(line)?;
    if values.len() < 7 || values.len() % 2 == 0 {
        return Err("YOLO polygon line must contain class and at least 3 points".to_string());
    }

    let class_id = parse_class_id(values[0])?;
    if values[1..].iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err("YOLO polygon coordinates must be within [0,1]".to_string());
    }
    let polygon = values[1..]
        .chunks(2)
        .map(|point| Point {
            x: round1(point[0] * image_width as f64),
            y: round1(point[1] * image_height as f64),
        })
        .collect();

    Ok(ParsedPolygon { class_id, polygon })
}

pub fn line_to_annotation(
    line: &str,
    image_width: u32,
    image_height: u32,
    labels: &[String],
    index: usize,
    prefer_polygon: bool,
) -> Result<AnnotationObject, String> {
    if prefer_polygon {
        let parsed = parse_yolo_polygon_line(line, image_width, image_height)?;
        let label = labels
            .get(parsed.class_id as usize)
            .cloned()
            .unwrap_or_else(|| format!("class_{}", parsed.class_id));
        return Ok(AnnotationObject::polygon(
            format!("ann-{index}"),
            parsed.class_id,
            label,
            parsed.polygon,
        ));
    }

    let parsed = parse_yolo_bbox_line(line, image_width, image_height)?;
    let label = labels
        .get(parsed.class_id as usize)
        .cloned()
        .unwrap_or_else(|| format!("class_{}", parsed.class_id));
    Ok(AnnotationObject::bbox(
        format!("ann-{index}"),
        parsed.class_id,
        label,
        parsed.bbox,
    ))
}

pub fn annotations_to_yolo_lines(
    objects: &[AnnotationObject],
    image_width: u32,
    image_height: u32,
) -> Result<String, String> {
    if image_width == 0 || image_height == 0 {
        return Err("image dimensions are required for YOLO export".to_string());
    }

    let mut lines = String::new();
    for object in objects {
        if object.object_type != "bbox" {
            return Err(format!(
                "YOLO detection cannot represent annotation '{}' of type '{}'",
                object.id, object.object_type
            ));
        }
        let bbox = object.bbox.as_ref().ok_or_else(|| {
            format!("bbox annotation '{}' has no bbox", object.id)
        })?;
        let width = bbox.width.max(1.0).min(image_width as f64);
        let height = bbox.height.max(1.0).min(image_height as f64);
        let center_x = (bbox.x + width / 2.0).clamp(0.0, image_width as f64);
        let center_y = (bbox.y + height / 2.0).clamp(0.0, image_height as f64);
        lines.push_str(&format!(
            "{} {:.6} {:.6} {:.6} {:.6}\n",
            object.class_id,
            center_x / image_width as f64,
            center_y / image_height as f64,
            width / image_width as f64,
            height / image_height as f64,
        ));
    }
    Ok(lines)
}

pub fn annotations_to_yolo_polygon_lines(
    objects: &[AnnotationObject],
    image_width: u32,
    image_height: u32,
) -> Result<String, String> {
    if image_width == 0 || image_height == 0 {
        return Err("image dimensions are required for YOLO export".to_string());
    }

    let mut lines = String::new();
    for object in objects {
        if object.object_type != "polygon" {
            return Err(format!(
                "YOLO segmentation cannot represent annotation '{}' of type '{}'",
                object.id, object.object_type
            ));
        }
        let polygon = object.polygon.as_ref().ok_or_else(|| {
            format!("polygon annotation '{}' has no points", object.id)
        })?;
        if polygon.len() < 3 {
            return Err(format!(
                "polygon annotation '{}' must contain at least 3 points",
                object.id
            ));
        }

        lines.push_str(&object.class_id.to_string());
        for point in polygon {
            let x = point.x.clamp(0.0, image_width as f64) / image_width as f64;
            let y = point.y.clamp(0.0, image_height as f64) / image_height as f64;
            lines.push_str(&format!(" {x:.6} {y:.6}"));
        }
        lines.push('\n');
    }
    Ok(lines)
}

fn parse_f64_values(line: &str) -> Result<Vec<f64>, String> {
    line.split_whitespace()
        .map(|part| {
            part.parse::<f64>()
                .map_err(|err| format!("invalid YOLO number '{part}': {err}"))
        })
        .collect()
}

fn parse_class_id(value: f64) -> Result<u32, String> {
    if !value.is_finite() || value < 0.0 || value > u32::MAX as f64 || value.fract() != 0.0 {
        return Err(format!("YOLO class id must be a non-negative integer: {value}"));
    }
    Ok(value as u32)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_fractional_or_negative_class_ids() {
        assert!(parse_yolo_bbox_line("1.5 0.5 0.5 0.2 0.2", 100, 100).is_err());
        assert!(parse_yolo_polygon_line("-1 0.1 0.1 0.5 0.1 0.5 0.5", 100, 100).is_err());
    }

    #[test]
    fn detection_export_rejects_polygon_instead_of_silently_dropping_it() {
        let objects = vec![AnnotationObject::polygon(
            "poly-1".to_string(),
            0,
            "region".to_string(),
            vec![Point { x: 1.0, y: 1.0 }, Point { x: 5.0, y: 1.0 }, Point { x: 1.0, y: 5.0 }],
        )];
        let error = annotations_to_yolo_lines(&objects, 16, 16).unwrap_err();
        assert!(error.contains("poly-1"), "{error}");
    }

    #[test]
    fn segmentation_export_rejects_bbox_instead_of_silently_dropping_it() {
        let objects = vec![AnnotationObject::bbox(
            "box-1".to_string(),
            0,
            "object".to_string(),
            BBox { x: 1.0, y: 1.0, width: 4.0, height: 4.0 },
        )];
        let error = annotations_to_yolo_polygon_lines(&objects, 16, 16).unwrap_err();
        assert!(error.contains("box-1"), "{error}");
    }
}
