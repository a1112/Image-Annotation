use super::{
    adapter::{
        source_version, verify_source_version_for_prepare, write_replacing, PrepareSourceSyncError,
        PreparedSourceSync, SourceSyncResult,
    },
    yolo,
};
use crate::domain::AnnotationObject;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub fn annotation_path(root: &Path, image_path: &Path) -> PathBuf {
    let Ok(relative) = image_path.strip_prefix(root) else {
        return image_path.with_extension("txt");
    };
    let mut parts: Vec<_> = relative.components().collect();
    if let Some(image_index) = parts.iter().position(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("images")
    }) {
        parts[image_index] = Component::Normal(std::ffi::OsStr::new("labels"));
        let mut label = root.to_path_buf();
        for component in parts {
            label.push(component.as_os_str());
        }
        label.set_extension("txt");
        return label;
    }
    image_path.with_extension("txt")
}

pub fn load_annotations(
    root: &Path,
    image_path: &Path,
    format: &str,
    labels: &[String],
) -> Result<Vec<AnnotationObject>, String> {
    let label_path = annotation_path(root, image_path);
    if !label_path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(&label_path).map_err(|err| err.to_string())?;
    let (width, height) = image::image_dimensions(image_path).map_err(|err| err.to_string())?;
    data.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| {
            yolo::line_to_annotation(line, width, height, labels, index, format == "yolo-seg")
                .map_err(|error| format!("{}:{}: {error}", label_path.display(), index + 1))
        })
        .collect()
}

pub fn sync_annotations(
    root: &Path,
    image_path: &Path,
    format: &str,
    objects: &[AnnotationObject],
    expected_version: Option<&str>,
) -> Result<SourceSyncResult, String> {
    let prepared = prepare_annotations(root, image_path, format, objects, expected_version)
        .map_err(|error| error.to_string())?;
    write_replacing(&prepared.path, &prepared.data)
}

pub fn prepare_annotations(
    root: &Path,
    image_path: &Path,
    format: &str,
    objects: &[AnnotationObject],
    expected_version: Option<&str>,
) -> Result<PreparedSourceSync, PrepareSourceSyncError> {
    let path = annotation_path(root, image_path);
    verify_source_version_for_prepare(&path, expected_version)?;
    let (width, height) = image::image_dimensions(image_path)
        .map_err(|error| PrepareSourceSyncError::Storage(error.to_string()))?;
    let data = match format {
        "yolo-detect" => yolo::annotations_to_yolo_lines(objects, width, height),
        "yolo-seg" => yolo::annotations_to_yolo_polygon_lines(objects, width, height),
        _ => Err(format!(
            "source synchronization is not implemented for {format}"
        )),
    }
    .map_err(PrepareSourceSyncError::Storage)?;
    Ok(PreparedSourceSync {
        path,
        data: data.into_bytes(),
    })
}

pub fn current_source_version(root: &Path, image_path: &Path) -> String {
    source_version(&annotation_path(root, image_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::BBox;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn yolo_adapter_loads_and_atomically_writes_nested_detection_labels() {
        let root = temp_root("yolo-adapter");
        let image_path = root.join("images").join("train").join("a.png");
        let label_path = root.join("labels").join("train").join("a.txt");
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        fs::create_dir_all(label_path.parent().unwrap()).unwrap();
        image::RgbaImage::new(100, 100).save(&image_path).unwrap();
        fs::write(&label_path, "1 0.500000 0.500000 0.400000 0.200000\n").unwrap();
        let labels = vec!["defect".to_string(), "scratch".to_string()];

        let mut objects = load_annotations(&root, &image_path, "yolo-detect", &labels).unwrap();
        assert_eq!(objects[0].label, "scratch");
        objects[0].bbox = Some(BBox {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 40.0,
        });

        let result = sync_annotations(&root, &image_path, "yolo-detect", &objects, None).unwrap();
        let reparsed = load_annotations(&root, &image_path, "yolo-detect", &labels).unwrap();

        assert_eq!(result.path, label_path);
        assert!(!result.source_version.is_empty());
        let expected = objects[0].bbox.as_ref().unwrap();
        let actual = reparsed[0].bbox.as_ref().unwrap();
        assert_eq!(actual.x, expected.x);
        assert_eq!(actual.y, expected.y);
        assert_eq!(actual.width, expected.width);
        assert_eq!(actual.height, expected.height);
        assert!(!root.join("labels").join("train").join("a.txt.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yolo_adapter_round_trips_segmentation_polygons() {
        let root = temp_root("yolo-seg-adapter");
        let image_path = root.join("images").join("train").join("a.png");
        let label_path = root.join("labels").join("train").join("a.txt");
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        fs::create_dir_all(label_path.parent().unwrap()).unwrap();
        image::RgbaImage::new(100, 200).save(&image_path).unwrap();
        fs::write(
            &label_path,
            "2 0.100000 0.100000 0.800000 0.100000 0.500000 0.450000\n",
        )
        .unwrap();
        let labels = vec![
            "defect".to_string(),
            "region".to_string(),
            "scratch".to_string(),
        ];

        let objects = load_annotations(&root, &image_path, "yolo-seg", &labels).unwrap();
        let result = sync_annotations(&root, &image_path, "yolo-seg", &objects, None).unwrap();
        let reparsed = load_annotations(&root, &image_path, "yolo-seg", &labels).unwrap();

        assert_eq!(result.path, label_path);
        assert_eq!(reparsed[0].label, "scratch");
        assert_eq!(reparsed[0].polygon.as_ref().unwrap().len(), 3);
        assert_eq!(
            fs::read_to_string(result.path).unwrap(),
            "2 0.100000 0.100000 0.800000 0.100000 0.500000 0.450000\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yolo_prepare_reports_a_stale_precheck_as_a_conflict_without_writing() {
        let root = temp_root("yolo-prepare-conflict");
        let image_path = root.join("images").join("train").join("a.png");
        let label_path = root.join("labels").join("train").join("a.txt");
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        fs::create_dir_all(label_path.parent().unwrap()).unwrap();
        image::RgbaImage::new(100, 100).save(&image_path).unwrap();
        fs::write(&label_path, "0 0.500000 0.500000 0.400000 0.200000\n").unwrap();
        let expected = current_source_version(&root, &image_path);
        fs::write(&label_path, "external edit\n").unwrap();
        let object = AnnotationObject::bbox(
            "new".to_string(),
            0,
            "defect".to_string(),
            BBox {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
        );

        let error = prepare_annotations(
            &root,
            &image_path,
            "yolo-detect",
            &[object],
            Some(&expected),
        )
        .unwrap_err();

        assert_eq!(error, PrepareSourceSyncError::Conflict);
        assert_eq!(fs::read(&label_path).unwrap(), b"external edit\n");
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
