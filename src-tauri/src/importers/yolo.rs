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

    let class_id = values[0] as u32;
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

    let class_id = values[0] as u32;
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
        let Some(bbox) = object.bbox.as_ref() else {
            continue;
        };
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
        let Some(polygon) = object.polygon.as_ref() else {
            continue;
        };
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

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}
