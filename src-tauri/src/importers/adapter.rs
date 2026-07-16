use crate::domain;
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct SourceSelection {
    pub paths: Vec<PathBuf>,
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    pub source_kind: String,
}

impl SourceSelection {
    pub fn from_paths(paths: &[PathBuf]) -> Result<Self, String> {
        if paths.is_empty() {
            return Err("请选择文件夹或文件".to_string());
        }
        let source_kind = if paths.len() == 1 && paths[0].is_dir() {
            "folder"
        } else {
            "files"
        };
        let root = if source_kind == "folder" {
            fs::canonicalize(&paths[0]).unwrap_or_else(|_| paths[0].clone())
        } else {
            common_parent(paths)
                .unwrap_or_else(|| paths[0].parent().unwrap_or(Path::new("")).to_path_buf())
        };
        Ok(Self {
            paths: paths.to_vec(),
            files: collect_source_files(paths),
            root,
            source_kind: source_kind.to_string(),
        })
    }

    pub fn image_count(&self) -> u32 {
        self.files
            .iter()
            .filter(|path| domain::is_image_path(path))
            .count() as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionResult {
    pub format: String,
    pub confidence: u8,
    pub annotation_path: Option<PathBuf>,
    pub reason: String,
}

pub trait AnnotationFormatAdapter {
    fn format(&self) -> &'static str;
    fn detect(&self, selection: &SourceSelection) -> DetectionResult;
}

pub fn collect_source_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            files.extend(
                WalkDir::new(path)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                    .map(|entry| entry.path().to_path_buf()),
            );
        } else if path.is_file() {
            files.push(path.clone());
        }
    }
    files.sort();
    files
}

pub fn common_parent(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut parents = paths
        .iter()
        .filter_map(|path| path.parent())
        .map(Path::to_path_buf);
    let mut common = parents.next()?;
    for parent in parents {
        while !parent.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

pub fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .map(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
        .unwrap_or(false)
}
