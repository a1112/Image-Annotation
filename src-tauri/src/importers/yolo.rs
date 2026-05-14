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
