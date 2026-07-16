use super::adapter::{has_extension, AnnotationFormatAdapter, DetectionResult, SourceSelection};
use serde_json::Value;
use std::{fs, path::PathBuf};

pub fn detect_source(paths: &[PathBuf]) -> Result<DetectionResult, String> {
    let selection = SourceSelection::from_paths(paths)?;
    let adapters: [&dyn AnnotationFormatAdapter; 5] = [
        &CocoDetector,
        &LabelMeDetector,
        &VocDetector,
        &YoloDetector,
        &ImageDirectoryDetector,
    ];
    let mut detections = adapters
        .iter()
        .map(|adapter| adapter.detect(&selection))
        .filter(|result| result.confidence > 0)
        .collect::<Vec<_>>();
    detections.sort_by(|left, right| right.confidence.cmp(&left.confidence));
    let Some(best) = detections.first().cloned() else {
        return Ok(DetectionResult {
            format: "unknown".to_string(),
            confidence: 0,
            annotation_path: None,
            reason: "未发现支持的图片或标注结构".to_string(),
        });
    };
    if detections
        .get(1)
        .map(|next| next.confidence == best.confidence && next.format != best.format)
        .unwrap_or(false)
    {
        return Ok(DetectionResult {
            format: "unknown".to_string(),
            confidence: best.confidence,
            annotation_path: None,
            reason: format!("数据源同时匹配 {} 和 {}", best.format, detections[1].format),
        });
    }
    Ok(best)
}

struct CocoDetector;

impl AnnotationFormatAdapter for CocoDetector {
    fn format(&self) -> &'static str {
        "coco"
    }

    fn detect(&self, selection: &SourceSelection) -> DetectionResult {
        for path in selection
            .files
            .iter()
            .filter(|path| has_extension(path, "json"))
        {
            let Some(value) = read_json(path) else {
                continue;
            };
            if value.get("images").and_then(Value::as_array).is_some()
                && value.get("annotations").and_then(Value::as_array).is_some()
                && value.get("categories").and_then(Value::as_array).is_some()
            {
                return detected(self.format(), 100, Some(path.clone()), "COCO JSON 结构");
            }
        }
        not_detected(self.format())
    }
}

struct LabelMeDetector;

impl AnnotationFormatAdapter for LabelMeDetector {
    fn format(&self) -> &'static str {
        "labelme"
    }

    fn detect(&self, selection: &SourceSelection) -> DetectionResult {
        for path in selection
            .files
            .iter()
            .filter(|path| has_extension(path, "json"))
        {
            let Some(value) = read_json(path) else {
                continue;
            };
            if value.get("imagePath").and_then(Value::as_str).is_some()
                && value.get("shapes").and_then(Value::as_array).is_some()
            {
                return detected(
                    self.format(),
                    95,
                    Some(path.clone()),
                    "LabelMe imagePath/shapes 结构",
                );
            }
        }
        not_detected(self.format())
    }
}

struct VocDetector;

impl AnnotationFormatAdapter for VocDetector {
    fn format(&self) -> &'static str {
        "voc-detect"
    }

    fn detect(&self, selection: &SourceSelection) -> DetectionResult {
        for path in selection
            .files
            .iter()
            .filter(|path| has_extension(path, "xml"))
        {
            let Ok(xml) = fs::read_to_string(path) else {
                continue;
            };
            if xml.contains("<annotation") && xml.contains("<filename>") && xml.contains("<size>") {
                return detected(
                    self.format(),
                    95,
                    Some(path.clone()),
                    "Pascal VOC annotation XML",
                );
            }
        }
        not_detected(self.format())
    }
}

struct YoloDetector;

impl AnnotationFormatAdapter for YoloDetector {
    fn format(&self) -> &'static str {
        "yolo"
    }

    fn detect(&self, selection: &SourceSelection) -> DetectionResult {
        let mut bbox_lines = 0u32;
        let mut polygon_lines = 0u32;
        let mut first_label = None;
        let mut first_bbox = None;
        let mut first_polygon = None;
        for path in selection
            .files
            .iter()
            .filter(|path| is_yolo_label_candidate(path))
        {
            let Ok(data) = fs::read_to_string(path) else {
                continue;
            };
            for (line_index, line) in data.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let values = line
                    .split_whitespace()
                    .map(str::parse::<f64>)
                    .collect::<Result<Vec<_>, _>>();
                let Ok(values) = values else {
                    continue;
                };
                if values.len() == 5 {
                    bbox_lines += 1;
                    first_label.get_or_insert_with(|| path.clone());
                    first_bbox.get_or_insert_with(|| (path.clone(), line_index + 1));
                } else if values.len() >= 7 && values.len() % 2 == 1 {
                    polygon_lines += 1;
                    first_label.get_or_insert_with(|| path.clone());
                    first_polygon.get_or_insert_with(|| (path.clone(), line_index + 1));
                }
            }
        }
        match (bbox_lines, polygon_lines) {
            (bbox, 0) if bbox > 0 => detected(
                "yolo-detect",
                90,
                first_label,
                "YOLO class + 4 normalized values",
            ),
            (0, polygon) if polygon > 0 => detected(
                "yolo-seg",
                90,
                first_label,
                "YOLO class + polygon point pairs",
            ),
            (bbox, polygon) if bbox > 0 && polygon > 0 => {
                let (path, line) = first_polygon.or(first_bbox).unwrap();
                detected(
                    "unknown",
                    40,
                    first_label,
                    &format!("YOLO 标签混合了 BBox 和 Polygon: {}:{line}", path.display()),
                )
            }
            _ => not_detected(self.format()),
        }
    }
}

struct ImageDirectoryDetector;

impl AnnotationFormatAdapter for ImageDirectoryDetector {
    fn format(&self) -> &'static str {
        "image-directory"
    }

    fn detect(&self, selection: &SourceSelection) -> DetectionResult {
        if selection.image_count() > 0 {
            detected(
                self.format(),
                20,
                None,
                "发现图片但没有更高置信度的标注格式",
            )
        } else {
            not_detected(self.format())
        }
    }
}

fn read_json(path: &PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn is_yolo_label_candidate(path: &PathBuf) -> bool {
    if !has_extension(path, "txt") {
        return false;
    }
    !matches!(
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .as_deref(),
        Some("classes.txt" | "obj.names")
    )
}

fn detected(
    format: &str,
    confidence: u8,
    annotation_path: Option<PathBuf>,
    reason: &str,
) -> DetectionResult {
    DetectionResult {
        format: format.to_string(),
        confidence,
        annotation_path,
        reason: reason.to_string(),
    }
}

fn not_detected(format: &str) -> DetectionResult {
    detected(format, 0, None, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn format_detection_recognizes_supported_dataset_layouts() {
        let cases = [
            ("yolo-detect", create_yolo_fixture(false)),
            ("yolo-seg", create_yolo_fixture(true)),
            ("voc-detect", create_voc_fixture()),
            ("coco", create_coco_fixture()),
            ("labelme", create_labelme_fixture()),
            ("image-directory", create_image_fixture()),
        ];

        for (expected, root) in &cases {
            let detection = detect_source(std::slice::from_ref(root)).unwrap();
            assert_eq!(detection.format, *expected, "fixture: {}", root.display());
            assert!(detection.confidence > 0);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn json_detection_uses_document_structure() {
        let coco = create_coco_fixture();
        let labelme = create_labelme_fixture();

        assert_eq!(
            detect_source(std::slice::from_ref(&coco)).unwrap().format,
            "coco"
        );
        assert_eq!(
            detect_source(std::slice::from_ref(&labelme))
                .unwrap()
                .format,
            "labelme"
        );

        let _ = fs::remove_dir_all(coco);
        let _ = fs::remove_dir_all(labelme);
    }

    #[test]
    fn mixed_yolo_detection_reports_conflicting_file_and_line() {
        let root = create_yolo_fixture(false);
        let label = root.join("labels").join("train").join("a.txt");
        fs::write(&label, "0 0.5 0.5 0.4 0.3\n0 0.1 0.1 0.8 0.1 0.5 0.9\n").unwrap();

        let detection = detect_source(std::slice::from_ref(&root)).unwrap();

        assert_eq!(detection.format, "unknown");
        assert!(detection.reason.contains("a.txt:2"));
        let _ = fs::remove_dir_all(root);
    }

    fn create_yolo_fixture(segmentation: bool) -> PathBuf {
        let root = temp_root(if segmentation {
            "detect-yolo-seg"
        } else {
            "detect-yolo-bbox"
        });
        fs::create_dir_all(root.join("images").join("train")).unwrap();
        fs::create_dir_all(root.join("labels").join("train")).unwrap();
        fs::write(root.join("images").join("train").join("a.jpg"), []).unwrap();
        let label = if segmentation {
            "0 0.1 0.1 0.8 0.1 0.5 0.9\n"
        } else {
            "0 0.5 0.5 0.4 0.3\n"
        };
        fs::write(root.join("labels").join("train").join("a.txt"), label).unwrap();
        root
    }

    fn create_voc_fixture() -> PathBuf {
        let root = temp_root("detect-voc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.jpg"), []).unwrap();
        fs::write(
            root.join("a.xml"),
            "<annotation><filename>a.jpg</filename><size><width>1</width><height>1</height></size></annotation>",
        )
        .unwrap();
        root
    }

    fn create_coco_fixture() -> PathBuf {
        let root = temp_root("detect-coco");
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("images").join("a.jpg"), []).unwrap();
        fs::write(
            root.join("annotations.json"),
            r#"{"images":[{"id":1,"file_name":"images/a.jpg"}],"annotations":[],"categories":[]}"#,
        )
        .unwrap();
        root
    }

    fn create_labelme_fixture() -> PathBuf {
        let root = temp_root("detect-labelme");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.jpg"), []).unwrap();
        fs::write(
            root.join("a.json"),
            r#"{"imagePath":"a.jpg","imageHeight":1,"imageWidth":1,"shapes":[]}"#,
        )
        .unwrap();
        root
    }

    fn create_image_fixture() -> PathBuf {
        let root = temp_root("detect-images");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.jpg"), []).unwrap();
        root
    }

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("image-annotation-{name}-{unique}"))
    }

    #[allow(dead_code)]
    fn _assert_path(_: &Path) {}
}
