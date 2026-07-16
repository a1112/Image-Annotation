use super::{
    adapter::{source_version, verify_source_version, write_replacing, SourceSyncResult},
    voc,
};
use crate::domain::AnnotationObject;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub fn annotation_path(root: &Path, image_path: &Path) -> PathBuf {
    let sidecar = image_path.with_extension("xml");
    if sidecar.exists() {
        return sidecar;
    }
    let Ok(relative) = image_path.strip_prefix(root) else {
        return sidecar;
    };
    let mut parts: Vec<_> = relative.components().collect();
    if let Some(image_index) = parts.iter().position(|component| {
        matches!(
            component
                .as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase()
                .as_str(),
            "jpegimages" | "images"
        )
    }) {
        parts[image_index] = Component::Normal(std::ffi::OsStr::new("Annotations"));
        let mut annotation = root.to_path_buf();
        for component in parts {
            annotation.push(component.as_os_str());
        }
        annotation.set_extension("xml");
        return annotation;
    }
    sidecar
}

pub fn load_annotations(
    root: &Path,
    image_path: &Path,
    labels: &[String],
) -> Result<Vec<AnnotationObject>, String> {
    let path = annotation_path(root, image_path);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let xml = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    voc::parse_voc_annotations(&xml, labels).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn sync_annotations(
    root: &Path,
    image_path: &Path,
    objects: &[AnnotationObject],
    expected_version: Option<&str>,
) -> Result<SourceSyncResult, String> {
    let path = annotation_path(root, image_path);
    verify_source_version(&path, expected_version)?;
    let (width, height) = image::image_dimensions(image_path).map_err(|err| err.to_string())?;
    let xml = voc::annotations_to_voc_xml(image_path, width, height, objects)?;
    write_replacing(&path, xml.as_bytes())
}

pub fn current_source_version(root: &Path, image_path: &Path) -> String {
    source_version(&annotation_path(root, image_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn voc_adapter_resolves_split_layout_and_preserves_pose() {
        let root = temp_root("voc-adapter");
        let image_path = root.join("JPEGImages").join("a.png");
        let annotation_path = root.join("Annotations").join("a.xml");
        fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        fs::create_dir_all(annotation_path.parent().unwrap()).unwrap();
        image::RgbaImage::new(100, 100).save(&image_path).unwrap();
        fs::write(
            &annotation_path,
            r#"
            <annotation>
              <filename>a.png</filename>
              <size><width>100</width><height>100</height><depth>3</depth></size>
              <object>
                <name>defect</name>
                <pose>Left</pose>
                <truncated>1</truncated>
                <difficult>0</difficult>
                <bndbox><xmin>10</xmin><ymin>20</ymin><xmax>40</xmax><ymax>60</ymax></bndbox>
              </object>
            </annotation>
            "#,
        )
        .unwrap();

        let labels = vec!["defect".to_string()];
        let objects = load_annotations(&root, &image_path, &labels).unwrap();
        assert_eq!(objects[0].attributes["pose"], serde_json::json!("Left"));

        let result = sync_annotations(&root, &image_path, &objects, None).unwrap();
        let xml = fs::read_to_string(&annotation_path).unwrap();

        assert_eq!(result.path, annotation_path);
        assert!(xml.contains("<pose>Left</pose>"));
        assert!(xml.contains("<truncated>1</truncated>"));
        assert!(!root.join("Annotations").join("a.xml.tmp").exists());
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
