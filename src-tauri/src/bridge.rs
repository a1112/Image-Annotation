use crate::{project_fs::ProjectManifest, storage::StoredClass};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub const BRIDGE_SCHEMA_VERSION: &str = "visualai.image-annotation.snapshot/v1";
const BRIDGE_ANNOTATION_FORMAT: &str = "visualai.normalized/v1";
const BRIDGE_MANIFEST_FILE_NAME: &str = "visualai-bridge.json";
const BRIDGE_TEMPORARY_FILE_NAME: &str = ".visualai-bridge.json.tmp";
const HASH_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_UNIX_TIMESTAMP_DIGITS: usize = 19;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BridgeManifest {
    #[serde(deserialize_with = "deserialize_schema_version")]
    pub schema_version: String,
    #[serde(deserialize_with = "deserialize_stable_id")]
    pub project_id: String,
    #[serde(deserialize_with = "deserialize_stable_id")]
    pub snapshot_id: String,
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    pub snapshot_name: String,
    #[serde(deserialize_with = "deserialize_unix_timestamp")]
    pub created_at: String,
    pub task_type: BridgeTaskType,
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    pub annotation_format: String,
    #[serde(deserialize_with = "deserialize_safe_relative_path")]
    pub asset_root: String,
    pub classes: Vec<BridgeClass>,
    pub samples: Vec<BridgeSample>,
}

impl BridgeManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != BRIDGE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported bridge schema version: {}",
                self.schema_version
            ));
        }
        validate_stable_id(&self.project_id).map_err(|error| format!("project_id: {error}"))?;
        validate_stable_id(&self.snapshot_id).map_err(|error| format!("snapshot_id: {error}"))?;
        validate_non_empty_string(&self.snapshot_name)
            .map_err(|error| format!("snapshot_name: {error}"))?;
        validate_unix_timestamp(&self.created_at)
            .map_err(|error| format!("created_at: {error}"))?;
        validate_non_empty_string(&self.annotation_format)
            .map_err(|error| format!("annotation_format: {error}"))?;
        if self.annotation_format != BRIDGE_ANNOTATION_FORMAT {
            return Err(format!(
                "unsupported bridge annotation format: {}",
                self.annotation_format
            ));
        }
        validate_safe_relative_path(&self.asset_root)
            .map_err(|error| format!("asset_root: {error}"))?;

        let mut class_ids = BTreeSet::new();
        for (class_index, class) in self.classes.iter().enumerate() {
            validate_stable_id(&class.id)
                .map_err(|error| format!("classes[{class_index}].id: {error}"))?;
            validate_non_empty_string(&class.label)
                .map_err(|error| format!("classes[{class_index}].label: {error}"))?;
            if !class_ids.insert(class.id.as_str()) {
                return Err(format!("duplicate class id: {}", class.id));
            }
        }

        let mut sample_ids = BTreeSet::new();
        let mut sample_paths = BTreeSet::new();
        for (sample_index, sample) in self.samples.iter().enumerate() {
            let prefix = format!("samples[{sample_index}]");
            validate_stable_id(&sample.id).map_err(|error| format!("{prefix}.id: {error}"))?;
            if !sample_ids.insert(sample.id.as_str()) {
                return Err(format!("duplicate sample id: {}", sample.id));
            }
            validate_safe_relative_path(&sample.relative_path)
                .map_err(|error| format!("{prefix}.relative_path: {error}"))?;
            if !sample_paths.insert(sample.relative_path.as_str()) {
                return Err(format!(
                    "duplicate sample relative_path: {}",
                    sample.relative_path
                ));
            }
            validate_positive_u32(sample.width)
                .map_err(|error| format!("{prefix}.width: {error}"))?;
            validate_positive_u32(sample.height)
                .map_err(|error| format!("{prefix}.height: {error}"))?;
            validate_positive_u64(sample.size_bytes)
                .map_err(|error| format!("{prefix}.size_bytes: {error}"))?;
            validate_sha256(&sample.sha256).map_err(|error| format!("{prefix}.sha256: {error}"))?;
            if let Some(revision) = &sample.revision {
                validate_stable_id(revision)
                    .map_err(|error| format!("{prefix}.revision: {error}"))?;
            }
            if let Some(split) = &sample.split {
                match split {
                    BridgeSplit::Train | BridgeSplit::Val | BridgeSplit::Test => {}
                }
            }

            let mut object_ids = BTreeSet::new();
            for (object_index, object) in sample.objects.iter().enumerate() {
                let object_prefix = format!("{prefix}.objects[{object_index}]");
                let object_id = bridge_object_id(object);
                if !object_ids.insert(object_id) {
                    return Err(format!(
                        "duplicate object id '{}' in sample '{}'",
                        object_id, sample.id
                    ));
                }
                let class_id = bridge_object_class_id(object);
                if !class_ids.contains(class_id) {
                    return Err(format!(
                        "object '{}' references unknown class '{}'",
                        object_id, class_id
                    ));
                }
                if !bridge_object_matches_task(&self.task_type, object) {
                    return Err(format!(
                        "object '{}' is incompatible with task {:?}",
                        object_id, self.task_type
                    ));
                }
                match object {
                    BridgeObject::Bbox {
                        id,
                        class_id,
                        x,
                        y,
                        width,
                        height,
                    } => {
                        validate_stable_id(id)
                            .map_err(|error| format!("{object_prefix}.id: {error}"))?;
                        validate_stable_id(class_id)
                            .map_err(|error| format!("{object_prefix}.class_id: {error}"))?;
                        validate_non_negative_f64(*x)
                            .map_err(|error| format!("{object_prefix}.x: {error}"))?;
                        validate_non_negative_f64(*y)
                            .map_err(|error| format!("{object_prefix}.y: {error}"))?;
                        validate_positive_f64(*width)
                            .map_err(|error| format!("{object_prefix}.width: {error}"))?;
                        validate_positive_f64(*height)
                            .map_err(|error| format!("{object_prefix}.height: {error}"))?;
                    }
                    BridgeObject::Classification { id, class_id } => {
                        validate_stable_id(id)
                            .map_err(|error| format!("{object_prefix}.id: {error}"))?;
                        validate_stable_id(class_id)
                            .map_err(|error| format!("{object_prefix}.class_id: {error}"))?;
                    }
                    BridgeObject::Polygon {
                        id,
                        class_id,
                        points,
                    } => {
                        validate_stable_id(id)
                            .map_err(|error| format!("{object_prefix}.id: {error}"))?;
                        validate_stable_id(class_id)
                            .map_err(|error| format!("{object_prefix}.class_id: {error}"))?;
                        validate_polygon_points(points)
                            .map_err(|error| format!("{object_prefix}.points: {error}"))?;
                    }
                }
            }
        }

        Ok(())
    }
}

impl Serialize for BridgeManifest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate()
            .map_err(<S::Error as serde::ser::Error>::custom)?;

        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        struct ValidatedManifest<'a> {
            schema_version: &'a str,
            project_id: &'a str,
            snapshot_id: &'a str,
            snapshot_name: &'a str,
            created_at: &'a str,
            task_type: &'a BridgeTaskType,
            annotation_format: &'a str,
            asset_root: &'a str,
            classes: &'a [BridgeClass],
            samples: &'a [BridgeSample],
        }

        ValidatedManifest {
            schema_version: &self.schema_version,
            project_id: &self.project_id,
            snapshot_id: &self.snapshot_id,
            snapshot_name: &self.snapshot_name,
            created_at: &self.created_at,
            task_type: &self.task_type,
            annotation_format: &self.annotation_format,
            asset_root: &self.asset_root,
            classes: &self.classes,
            samples: &self.samples,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeTaskType {
    Detection,
    Classification,
    Segmentation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BridgeClass {
    #[serde(deserialize_with = "deserialize_stable_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BridgeSample {
    #[serde(deserialize_with = "deserialize_stable_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_safe_relative_path")]
    pub relative_path: String,
    #[serde(deserialize_with = "deserialize_positive_u32")]
    pub width: u32,
    #[serde(deserialize_with = "deserialize_positive_u32")]
    pub height: u32,
    #[serde(deserialize_with = "deserialize_positive_u64")]
    pub size_bytes: u64,
    #[serde(deserialize_with = "deserialize_sha256")]
    pub sha256: String,
    pub split: Option<BridgeSplit>,
    #[serde(default, deserialize_with = "deserialize_optional_stable_id")]
    pub revision: Option<String>,
    pub objects: Vec<BridgeObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeSplit {
    Train,
    Val,
    Test,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum BridgeObject {
    Bbox {
        #[serde(deserialize_with = "deserialize_stable_id")]
        id: String,
        #[serde(deserialize_with = "deserialize_stable_id")]
        class_id: String,
        #[serde(deserialize_with = "deserialize_non_negative_f64")]
        x: f64,
        #[serde(deserialize_with = "deserialize_non_negative_f64")]
        y: f64,
        #[serde(deserialize_with = "deserialize_positive_f64")]
        width: f64,
        #[serde(deserialize_with = "deserialize_positive_f64")]
        height: f64,
    },
    Classification {
        #[serde(deserialize_with = "deserialize_stable_id")]
        id: String,
        #[serde(deserialize_with = "deserialize_stable_id")]
        class_id: String,
    },
    Polygon {
        #[serde(deserialize_with = "deserialize_stable_id")]
        id: String,
        #[serde(deserialize_with = "deserialize_stable_id")]
        class_id: String,
        #[serde(deserialize_with = "deserialize_polygon_points")]
        points: Vec<BridgePoint>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BridgePoint {
    #[serde(deserialize_with = "deserialize_non_negative_f64")]
    pub x: f64,
    #[serde(deserialize_with = "deserialize_non_negative_f64")]
    pub y: f64,
}

pub struct BridgeBuildInput<'a> {
    pub project: &'a ProjectManifest,
    pub snapshot_id: &'a str,
    pub snapshot_name: &'a str,
    pub created_at: &'a str,
    pub asset_root: &'a Path,
    pub classes: &'a [StoredClass],
    pub samples: &'a [BridgeSourceSample],
}

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeSourceSample {
    pub id: String,
    pub relative_path: String,
    pub width: u32,
    pub height: u32,
    pub split: Option<BridgeSplit>,
    pub revision: Option<String>,
    pub objects: Vec<BridgeObject>,
}

pub fn write_bridge_manifest(
    snapshot_dir: &Path,
    input: BridgeBuildInput<'_>,
) -> Result<PathBuf, String> {
    let final_path = snapshot_dir.join(BRIDGE_MANIFEST_FILE_NAME);
    let temporary_path = snapshot_dir.join(BRIDGE_TEMPORARY_FILE_NAME);
    remove_temporary_file(&temporary_path);

    let manifest = build_bridge_manifest(snapshot_dir, input)?;
    manifest.validate()?;
    if final_path.exists() {
        return Err(format!(
            "bridge manifest already exists: {}",
            final_path.display()
        ));
    }

    let publish_result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|error| {
                format!(
                    "create bridge temporary file {}: {error}",
                    temporary_path.display()
                )
            })?;
        serde_json::to_writer_pretty(&mut temporary, &manifest)
            .map_err(|error| format!("serialize bridge manifest: {error}"))?;
        temporary
            .write_all(b"\n")
            .map_err(|error| format!("finish bridge manifest: {error}"))?;
        temporary
            .flush()
            .map_err(|error| format!("flush bridge manifest: {error}"))?;
        temporary
            .sync_all()
            .map_err(|error| format!("sync bridge manifest: {error}"))?;
        drop(temporary);
        fs::rename(&temporary_path, &final_path).map_err(|error| {
            format!("publish bridge manifest {}: {error}", final_path.display())
        })?;
        Ok::<(), String>(())
    })();

    if let Err(error) = publish_result {
        remove_temporary_file(&temporary_path);
        return Err(error);
    }

    Ok(final_path)
}

fn build_bridge_manifest(
    snapshot_dir: &Path,
    input: BridgeBuildInput<'_>,
) -> Result<BridgeManifest, String> {
    let task_type = match input.project.format.as_str() {
        "yolo-detect" | "voc-detect" => BridgeTaskType::Detection,
        "image-classification" => BridgeTaskType::Classification,
        "yolo-seg" => BridgeTaskType::Segmentation,
        format => return Err(format!("unsupported bridge project format: {format}")),
    };
    let asset_root = relative_asset_root(snapshot_dir, input.asset_root)?;

    let mut classes = input
        .classes
        .iter()
        .map(|class| BridgeClass {
            id: class.id.to_string(),
            label: class.label.clone(),
        })
        .collect::<Vec<_>>();
    classes.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.label.cmp(&right.label))
    });

    let mut samples = Vec::with_capacity(input.samples.len());
    for source in input.samples {
        validate_safe_relative_path(&source.relative_path)
            .map_err(|error| format!("sample '{}': {error}", source.id))?;

        let asset_path = input.asset_root.join(Path::new(&source.relative_path));
        let (size_bytes, sha256) = stream_file_integrity(&asset_path)?;
        let mut objects = source.objects.clone();
        objects.sort_by(|left, right| {
            bridge_object_id(left)
                .cmp(bridge_object_id(right))
                .then_with(|| bridge_object_kind(left).cmp(&bridge_object_kind(right)))
        });
        samples.push(BridgeSample {
            id: source.id.clone(),
            relative_path: source.relative_path.clone(),
            width: source.width,
            height: source.height,
            size_bytes,
            sha256,
            split: source.split.clone(),
            revision: source.revision.clone(),
            objects,
        });
    }
    samples.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });

    Ok(BridgeManifest {
        schema_version: BRIDGE_SCHEMA_VERSION.to_string(),
        project_id: input.project.id.clone(),
        snapshot_id: input.snapshot_id.to_string(),
        snapshot_name: input.snapshot_name.to_string(),
        created_at: input.created_at.to_string(),
        task_type,
        annotation_format: BRIDGE_ANNOTATION_FORMAT.to_string(),
        asset_root,
        classes,
        samples,
    })
}

fn relative_asset_root(snapshot_dir: &Path, asset_root: &Path) -> Result<String, String> {
    let relative = asset_root.strip_prefix(snapshot_dir).map_err(|_| {
        format!(
            "bridge asset root {} must be inside snapshot directory {}",
            asset_root.display(),
            snapshot_dir.display()
        )
    })?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| "bridge asset root must be valid UTF-8".to_string()),
            _ => Err("bridge asset root must be a normalized relative path".to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let relative = components.join("/");
    validate_safe_relative_path(&relative).map_err(str::to_string)?;
    Ok(relative)
}

pub(crate) fn stream_file_integrity(path: &Path) -> Result<(u64, String), String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open bridge asset {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0u64;
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read bridge asset {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        size_bytes = size_bytes
            .checked_add(read as u64)
            .ok_or_else(|| format!("bridge asset is too large: {}", path.display()))?;
        hasher.update(&buffer[..read]);
    }
    let metadata_size = file
        .metadata()
        .map_err(|error| format!("read bridge asset metadata {}: {error}", path.display()))?
        .len();
    if size_bytes != metadata_size {
        return Err(format!(
            "bridge asset changed while hashing: {}",
            path.display()
        ));
    }
    validate_positive_u64(size_bytes)
        .map_err(|error| format!("bridge asset {}: {error}", path.display()))?;
    Ok((size_bytes, format!("{:x}", hasher.finalize())))
}

fn bridge_object_id(object: &BridgeObject) -> &str {
    match object {
        BridgeObject::Bbox { id, .. }
        | BridgeObject::Classification { id, .. }
        | BridgeObject::Polygon { id, .. } => id,
    }
}

fn bridge_object_class_id(object: &BridgeObject) -> &str {
    match object {
        BridgeObject::Bbox { class_id, .. }
        | BridgeObject::Classification { class_id, .. }
        | BridgeObject::Polygon { class_id, .. } => class_id,
    }
}

fn bridge_object_matches_task(task_type: &BridgeTaskType, object: &BridgeObject) -> bool {
    matches!(
        (task_type, object),
        (BridgeTaskType::Detection, BridgeObject::Bbox { .. })
            | (
                BridgeTaskType::Classification,
                BridgeObject::Classification { .. }
            )
            | (
                BridgeTaskType::Segmentation,
                BridgeObject::Bbox { .. } | BridgeObject::Polygon { .. }
            )
    )
}

fn bridge_object_kind(object: &BridgeObject) -> u8 {
    match object {
        BridgeObject::Bbox { .. } => 0,
        BridgeObject::Classification { .. } => 1,
        BridgeObject::Polygon { .. } => 2,
    }
}

fn remove_temporary_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

fn deserialize_schema_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value == BRIDGE_SCHEMA_VERSION {
        Ok(value)
    } else {
        Err(D::Error::custom(format!(
            "unsupported bridge schema version: {value}"
        )))
    }
}

fn deserialize_non_empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_non_empty_string(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_non_empty_string(value: &str) -> Result<(), &'static str> {
    let has_boundary_whitespace = value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace);
    if !value.is_empty()
        && !has_boundary_whitespace
        && value.chars().all(|character| !character.is_control())
    {
        Ok(())
    } else {
        Err("value must be non-empty, have no boundary whitespace, and contain no controls")
    }
}

fn validate_unix_timestamp(value: &str) -> Result<(), &'static str> {
    let canonical = value == "0"
        || (value.starts_with(|character: char| ('1'..='9').contains(&character))
            && value.bytes().all(|character| character.is_ascii_digit()));
    if canonical && value.len() <= MAX_UNIX_TIMESTAMP_DIGITS && value.parse::<u64>().is_ok() {
        Ok(())
    } else {
        Err("value must be a canonical decimal Unix-seconds string of at most 19 digits")
    }
}

fn deserialize_unix_timestamp<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_unix_timestamp(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn deserialize_stable_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_stable_id(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_stable_id(value: &str) -> Result<(), &'static str> {
    validate_non_empty_string(value)?;
    if value
        .chars()
        .all(|character| !character.is_control() && character != '/' && character != '\\')
    {
        Ok(())
    } else {
        Err("stable ID must not contain control characters or path separators")
    }
}

fn deserialize_optional_stable_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        validate_stable_id(value).map_err(D::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_safe_relative_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_safe_relative_path(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_safe_relative_path(value: &str) -> Result<(), &'static str> {
    let safe = !value.is_empty()
        && !value.starts_with('/')
        && !value.starts_with('\\')
        && !value.contains(':')
        && !value.contains('\\')
        && !value.contains('\0')
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..");
    if safe {
        Ok(())
    } else {
        Err("path must be a normalized, portable relative path")
    }
}

fn deserialize_positive_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    validate_positive_u32(value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_positive_u32(value: u32) -> Result<(), &'static str> {
    if value > 0 {
        Ok(())
    } else {
        Err("value must be greater than zero")
    }
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    validate_positive_u64(value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_positive_u64(value: u64) -> Result<(), &'static str> {
    if value > 0 && value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err("value must be between 1 and 9007199254740991")
    }
}

fn deserialize_sha256<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_sha256(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_sha256(value: &str) -> Result<(), &'static str> {
    if value.len() == 64
        && value
            .bytes()
            .all(|character| character.is_ascii_digit() || (b'a'..=b'f').contains(&character))
    {
        Ok(())
    } else {
        Err("sha256 must contain exactly 64 lowercase hexadecimal characters")
    }
}

fn deserialize_non_negative_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    validate_non_negative_f64(value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_non_negative_f64(value: f64) -> Result<(), &'static str> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err("value must be finite and greater than or equal to zero")
    }
}

fn deserialize_positive_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    validate_positive_f64(value).map_err(D::Error::custom)?;
    Ok(value)
}

fn validate_positive_f64(value: f64) -> Result<(), &'static str> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err("value must be finite and greater than zero")
    }
}

fn deserialize_polygon_points<'de, D>(deserializer: D) -> Result<Vec<BridgePoint>, D::Error>
where
    D: Deserializer<'de>,
{
    let points = Vec::<BridgePoint>::deserialize(deserializer)?;
    validate_polygon_points(&points).map_err(D::Error::custom)?;
    Ok(points)
}

fn validate_polygon_points(points: &[BridgePoint]) -> Result<(), &'static str> {
    if points.len() >= 3 {
        for point in points {
            validate_non_negative_f64(point.x)?;
            validate_non_negative_f64(point.y)?;
        }
        Ok(())
    } else {
        Err("polygon must contain at least three finite, non-negative points")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        write_bridge_manifest, BridgeBuildInput, BridgeClass, BridgeManifest, BridgeObject,
        BridgePoint, BridgeSample, BridgeSourceSample, BridgeSplit, BridgeTaskType,
        BRIDGE_SCHEMA_VERSION,
    };
    use crate::{project_fs::ProjectManifest, storage::StoredClass};
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_snapshot_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "image-annotation-bridge-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn test_project(root: &Path, format: &str) -> ProjectManifest {
        ProjectManifest {
            id: "project-bridge".to_string(),
            name: "Bridge Project".to_string(),
            source_dataset_key: "local".to_string(),
            format: format.to_string(),
            root_path: root.to_string_lossy().to_string(),
            created_at: "1785311000".to_string(),
            class_count: 2,
            image_count: 2,
        }
    }

    fn valid_manifest() -> BridgeManifest {
        BridgeManifest {
            schema_version: BRIDGE_SCHEMA_VERSION.to_string(),
            project_id: "project-1".to_string(),
            snapshot_id: "snapshot-1".to_string(),
            snapshot_name: "Training snapshot".to_string(),
            created_at: "1785312000".to_string(),
            task_type: BridgeTaskType::Detection,
            annotation_format: "visualai.normalized/v1".to_string(),
            asset_root: "assets".to_string(),
            classes: vec![BridgeClass {
                id: "0".to_string(),
                label: "person".to_string(),
            }],
            samples: vec![BridgeSample {
                id: "image-1".to_string(),
                relative_path: "images/street.jpg".to_string(),
                width: 640,
                height: 480,
                size_bytes: 12345,
                sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
                split: Some(BridgeSplit::Train),
                revision: Some("revision-1".to_string()),
                objects: vec![BridgeObject::Bbox {
                    id: "object-1".to_string(),
                    class_id: "0".to_string(),
                    x: 64.0,
                    y: 96.0,
                    width: 192.0,
                    height: 192.0,
                }],
            }],
        }
    }

    #[test]
    fn writes_bridge_manifest_with_streamed_image_hashes() {
        let snapshot_dir = temporary_snapshot_dir("writes");
        let asset_root = snapshot_dir.join("assets");
        fs::create_dir_all(asset_root.join("images")).unwrap();
        let image_a = b"first image bytes";
        let mut image_z = vec![0x5a; super::HASH_BUFFER_SIZE + 17];
        image_z[super::HASH_BUFFER_SIZE] = 0xa5;
        fs::write(asset_root.join("images/a.png"), image_a).unwrap();
        fs::write(asset_root.join("images/z.png"), &image_z).unwrap();

        let project = test_project(&asset_root, "yolo-detect");
        let classes = vec![
            StoredClass {
                id: 2,
                label: "zebra".to_string(),
                color: "#ffffff".to_string(),
            },
            StoredClass {
                id: 1,
                label: "antelope".to_string(),
                color: "#000000".to_string(),
            },
        ];
        let samples = vec![
            BridgeSourceSample {
                id: "sample-z".to_string(),
                relative_path: "images/z.png".to_string(),
                width: 20,
                height: 10,
                split: Some(BridgeSplit::Val),
                revision: Some("revision-z".to_string()),
                objects: vec![
                    BridgeObject::Bbox {
                        id: "object-z".to_string(),
                        class_id: "2".to_string(),
                        x: 2.0,
                        y: 3.0,
                        width: 4.0,
                        height: 5.0,
                    },
                    BridgeObject::Bbox {
                        id: "object-a".to_string(),
                        class_id: "1".to_string(),
                        x: 1.0,
                        y: 2.0,
                        width: 3.0,
                        height: 4.0,
                    },
                ],
            },
            BridgeSourceSample {
                id: "sample-a".to_string(),
                relative_path: "images/a.png".to_string(),
                width: 12,
                height: 8,
                split: Some(BridgeSplit::Train),
                revision: None,
                objects: Vec::new(),
            },
        ];

        let final_path = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-bridge-1",
                snapshot_name: "Bridge Snapshot",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &classes,
                samples: &samples,
            },
        )
        .unwrap();

        assert_eq!(final_path, snapshot_dir.join("visualai-bridge.json"));
        assert!(final_path.is_file());
        assert!(!snapshot_dir.join(".visualai-bridge.json.tmp").exists());
        let manifest: BridgeManifest =
            serde_json::from_str(&fs::read_to_string(&final_path).unwrap()).unwrap();
        assert_eq!(manifest.schema_version, BRIDGE_SCHEMA_VERSION);
        assert_eq!(manifest.asset_root, "assets");
        assert_eq!(
            manifest
                .classes
                .iter()
                .map(|class| class.id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2"]
        );
        assert_eq!(
            manifest
                .samples
                .iter()
                .map(|sample| sample.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sample-a", "sample-z"]
        );
        assert_eq!(manifest.samples[0].relative_path, "images/a.png");
        assert_eq!(manifest.samples[0].size_bytes, image_a.len() as u64);
        assert_eq!(
            manifest.samples[0].sha256,
            format!("{:x}", Sha256::digest(image_a))
        );
        assert_eq!(manifest.samples[1].relative_path, "images/z.png");
        assert_eq!(manifest.samples[1].size_bytes, image_z.len() as u64);
        assert_eq!(
            manifest.samples[1].sha256,
            format!("{:x}", Sha256::digest(&image_z))
        );
        assert_eq!(
            manifest.samples[1]
                .objects
                .iter()
                .map(|object| match object {
                    BridgeObject::Bbox { id, .. }
                    | BridgeObject::Classification { id, .. }
                    | BridgeObject::Polygon { id, .. } => id.as_str(),
                })
                .collect::<Vec<_>>(),
            vec!["object-a", "object-z"]
        );

        let published_bytes = fs::read(&final_path).unwrap();
        let second_publish = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-bridge-1",
                snapshot_name: "Bridge Snapshot",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &classes,
                samples: &samples,
            },
        );
        assert!(second_publish.is_err());
        assert_eq!(fs::read(&final_path).unwrap(), published_bytes);
        assert!(!snapshot_dir.join(".visualai-bridge.json.tmp").exists());

        fs::remove_dir_all(snapshot_dir).unwrap();
    }

    #[test]
    fn missing_asset_does_not_publish_bridge_manifest() {
        let snapshot_dir = temporary_snapshot_dir("missing");
        let asset_root = snapshot_dir.join("assets");
        fs::create_dir_all(&asset_root).unwrap();
        fs::write(
            snapshot_dir.join(".visualai-bridge.json.tmp"),
            b"stale partial manifest",
        )
        .unwrap();
        let project = test_project(&asset_root, "yolo-detect");
        let samples = vec![BridgeSourceSample {
            id: "missing-image".to_string(),
            relative_path: "images/missing.png".to_string(),
            width: 10,
            height: 10,
            split: None,
            revision: None,
            objects: Vec::new(),
        }];

        let result = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-missing",
                snapshot_name: "Missing Asset Snapshot",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &[],
                samples: &samples,
            },
        );

        assert!(result.is_err());
        assert!(!snapshot_dir.join("visualai-bridge.json").exists());
        assert!(!snapshot_dir.join(".visualai-bridge.json.tmp").exists());
        fs::remove_dir_all(snapshot_dir).unwrap();
    }

    #[test]
    fn maps_supported_project_formats_to_bridge_task_types() {
        for (format, expected) in [
            ("yolo-detect", BridgeTaskType::Detection),
            ("voc-detect", BridgeTaskType::Detection),
            ("image-classification", BridgeTaskType::Classification),
            ("yolo-seg", BridgeTaskType::Segmentation),
        ] {
            let snapshot_dir = temporary_snapshot_dir(format);
            let asset_root = snapshot_dir.join("assets");
            fs::create_dir_all(&asset_root).unwrap();
            let project = test_project(&asset_root, format);
            let final_path = write_bridge_manifest(
                &snapshot_dir,
                BridgeBuildInput {
                    project: &project,
                    snapshot_id: "snapshot-format",
                    snapshot_name: "Format Snapshot",
                    created_at: "1785312000",
                    asset_root: &asset_root,
                    classes: &[],
                    samples: &[],
                },
            )
            .unwrap();
            let manifest: BridgeManifest =
                serde_json::from_str(&fs::read_to_string(final_path).unwrap()).unwrap();
            assert_eq!(manifest.task_type, expected);
            assert_eq!(manifest.annotation_format, "visualai.normalized/v1");
            fs::remove_dir_all(snapshot_dir).unwrap();
        }
    }

    #[test]
    fn producer_rejects_objects_incompatible_with_project_task() {
        let snapshot_dir = temporary_snapshot_dir("incompatible-object");
        let asset_root = snapshot_dir.join("assets");
        fs::create_dir_all(asset_root.join("images")).unwrap();
        fs::write(asset_root.join("images/sample.png"), b"sample bytes").unwrap();
        let project = test_project(&asset_root, "image-classification");
        let classes = vec![StoredClass {
            id: 1,
            label: "class".to_string(),
            color: "#ffffff".to_string(),
        }];
        let samples = vec![BridgeSourceSample {
            id: "sample-1".to_string(),
            relative_path: "images/sample.png".to_string(),
            width: 10,
            height: 10,
            split: None,
            revision: None,
            objects: vec![BridgeObject::Bbox {
                id: "object-1".to_string(),
                class_id: "1".to_string(),
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }],
        }];

        let result = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-incompatible",
                snapshot_name: "Incompatible Object",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &classes,
                samples: &samples,
            },
        );

        assert!(result.is_err());
        assert!(!snapshot_dir.join("visualai-bridge.json").exists());
        assert!(!snapshot_dir.join(".visualai-bridge.json.tmp").exists());
        fs::remove_dir_all(snapshot_dir).unwrap();
    }

    #[test]
    fn producer_rejects_unknown_class_references() {
        let snapshot_dir = temporary_snapshot_dir("unknown-class");
        let asset_root = snapshot_dir.join("assets");
        fs::create_dir_all(asset_root.join("images")).unwrap();
        fs::write(asset_root.join("images/sample.png"), b"sample bytes").unwrap();
        let project = test_project(&asset_root, "yolo-detect");
        let classes = vec![StoredClass {
            id: 1,
            label: "class".to_string(),
            color: "#ffffff".to_string(),
        }];
        let samples = vec![BridgeSourceSample {
            id: "sample-1".to_string(),
            relative_path: "images/sample.png".to_string(),
            width: 10,
            height: 10,
            split: None,
            revision: None,
            objects: vec![BridgeObject::Bbox {
                id: "object-1".to_string(),
                class_id: "999".to_string(),
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }],
        }];

        let result = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-unknown-class",
                snapshot_name: "Unknown Class",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &classes,
                samples: &samples,
            },
        );

        assert!(result.is_err());
        assert!(!snapshot_dir.join("visualai-bridge.json").exists());
        assert!(!snapshot_dir.join(".visualai-bridge.json.tmp").exists());
        fs::remove_dir_all(snapshot_dir).unwrap();
    }

    #[test]
    fn segmentation_producer_preserves_bbox_and_polygon_objects() {
        let snapshot_dir = temporary_snapshot_dir("segmentation-objects");
        let asset_root = snapshot_dir.join("assets");
        fs::create_dir_all(asset_root.join("images")).unwrap();
        fs::write(asset_root.join("images/sample.png"), b"sample bytes").unwrap();
        let project = test_project(&asset_root, "yolo-seg");
        let classes = vec![StoredClass {
            id: 1,
            label: "region".to_string(),
            color: "#ffffff".to_string(),
        }];
        let samples = vec![BridgeSourceSample {
            id: "sample-1".to_string(),
            relative_path: "images/sample.png".to_string(),
            width: 10,
            height: 10,
            split: None,
            revision: None,
            objects: vec![
                BridgeObject::Polygon {
                    id: "polygon-1".to_string(),
                    class_id: "1".to_string(),
                    points: vec![
                        BridgePoint { x: 0.0, y: 0.0 },
                        BridgePoint { x: 2.0, y: 0.0 },
                        BridgePoint { x: 1.0, y: 2.0 },
                    ],
                },
                BridgeObject::Bbox {
                    id: "bbox-1".to_string(),
                    class_id: "1".to_string(),
                    x: 0.0,
                    y: 0.0,
                    width: 2.0,
                    height: 2.0,
                },
            ],
        }];

        let final_path = write_bridge_manifest(
            &snapshot_dir,
            BridgeBuildInput {
                project: &project,
                snapshot_id: "snapshot-segmentation",
                snapshot_name: "Segmentation",
                created_at: "1785312000",
                asset_root: &asset_root,
                classes: &classes,
                samples: &samples,
            },
        )
        .unwrap();
        let manifest: BridgeManifest =
            serde_json::from_str(&fs::read_to_string(final_path).unwrap()).unwrap();

        assert!(matches!(
            manifest.samples[0].objects[0],
            BridgeObject::Bbox { .. }
        ));
        assert!(matches!(
            manifest.samples[0].objects[1],
            BridgeObject::Polygon { .. }
        ));
        fs::remove_dir_all(snapshot_dir).unwrap();
    }

    #[test]
    fn shared_bridge_fixtures_parse_strictly() {
        for fixture in ["detection", "classification", "segmentation"] {
            let text = std::fs::read_to_string(format!(
                "{}/../docs/protocol/fixtures/{fixture}.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            let manifest: BridgeManifest = serde_json::from_str(&text).unwrap();
            assert_eq!(manifest.schema_version, BRIDGE_SCHEMA_VERSION);
        }
    }

    #[test]
    fn bridge_manifest_rejects_unknown_fields() {
        let mut invalid: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();
        invalid
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<BridgeManifest>(invalid).is_err());
    }

    #[test]
    fn bridge_manifest_rejects_unsafe_paths_and_invalid_integrity_fields() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();
        for (field, invalid_value) in [
            (
                "relative_path",
                serde_json::Value::String("../street.jpg".to_string()),
            ),
            ("width", serde_json::Value::Number(0.into())),
            ("sha256", serde_json::Value::String("ABCDEF".to_string())),
        ] {
            let mut invalid = fixture.clone();
            invalid["samples"][0][field] = invalid_value;
            assert!(serde_json::from_value::<BridgeManifest>(invalid).is_err());
        }
    }

    #[test]
    fn bridge_object_variants_reject_unknown_fields() {
        let mut invalid: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();
        invalid["samples"][0]["objects"][0]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<BridgeManifest>(invalid).is_err());
    }

    #[test]
    fn bridge_manifest_rejects_a_different_schema_version() {
        let mut invalid: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();
        invalid["schema_version"] =
            serde_json::Value::String("visualai.image-annotation.snapshot/v2".to_string());
        assert!(serde_json::from_value::<BridgeManifest>(invalid).is_err());
    }

    #[test]
    fn bridge_manifest_rejects_noncanonical_unix_timestamps() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();

        for invalid_created_at in [
            "2026-07-29T08:00:00Z",
            "",
            "01",
            "+1",
            "-1",
            "18446744073709551616",
        ] {
            let mut invalid = fixture.clone();
            invalid["created_at"] = serde_json::Value::String(invalid_created_at.to_string());
            assert!(
                serde_json::from_value::<BridgeManifest>(invalid).is_err(),
                "created_at {invalid_created_at:?} unexpectedly parsed"
            );
        }
    }

    #[test]
    fn invalid_directly_constructed_manifests_cannot_be_serialized() {
        let mut invalid_manifests = Vec::new();

        let mut dangerous_path = valid_manifest();
        dangerous_path.samples[0].relative_path = "../escape.jpg".to_string();
        invalid_manifests.push(("dangerous path", dangerous_path));

        let mut zero_width = valid_manifest();
        zero_width.samples[0].width = 0;
        invalid_manifests.push(("zero width", zero_width));

        let mut invalid_sha = valid_manifest();
        invalid_sha.samples[0].sha256 = "ABCDEF".to_string();
        invalid_manifests.push(("invalid sha256", invalid_sha));

        let mut oversized_file = valid_manifest();
        oversized_file.samples[0].size_bytes = 9_007_199_254_740_992;
        invalid_manifests.push(("unsafe integer file size", oversized_file));

        let mut non_finite_bbox = valid_manifest();
        non_finite_bbox.samples[0].objects = vec![BridgeObject::Bbox {
            id: "object-1".to_string(),
            class_id: "0".to_string(),
            x: f64::NAN,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }];
        invalid_manifests.push(("non-finite bbox", non_finite_bbox));

        let mut short_polygon = valid_manifest();
        short_polygon.samples[0].objects = vec![BridgeObject::Polygon {
            id: "object-1".to_string(),
            class_id: "0".to_string(),
            points: vec![
                BridgePoint { x: 0.0, y: 0.0 },
                BridgePoint { x: 1.0, y: 1.0 },
            ],
        }];
        invalid_manifests.push(("short polygon", short_polygon));

        let mut wrong_schema = valid_manifest();
        wrong_schema.schema_version = "visualai.image-annotation.snapshot/v2".to_string();
        invalid_manifests.push(("wrong schema version", wrong_schema));

        let mut wrong_timestamp = valid_manifest();
        wrong_timestamp.created_at = "2026-07-29T08:00:00Z".to_string();
        invalid_manifests.push(("noncanonical timestamp", wrong_timestamp));

        for (label, manifest) in invalid_manifests {
            assert!(
                manifest.validate().is_err(),
                "{label} unexpectedly validated"
            );
            assert!(
                serde_json::to_string(&manifest).is_err(),
                "{label} unexpectedly serialized"
            );
        }
    }

    #[test]
    fn valid_bridge_fixtures_can_be_serialized_again() {
        for fixture in ["detection", "classification", "segmentation"] {
            let text = std::fs::read_to_string(format!(
                "{}/../docs/protocol/fixtures/{fixture}.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            let manifest: BridgeManifest = serde_json::from_str(&text).unwrap();
            manifest.validate().unwrap();
            serde_json::to_string(&manifest).unwrap();
        }
    }

    #[test]
    fn manifest_strings_reject_controls_but_allow_internal_spaces() {
        let mut invalid_manifests = Vec::new();

        let mut snapshot_lf = valid_manifest();
        snapshot_lf.snapshot_name = "Training\nsnapshot".to_string();
        invalid_manifests.push(("snapshot_name internal LF", snapshot_lf));

        let mut class_cr = valid_manifest();
        class_cr.classes[0].label = "per\rson".to_string();
        invalid_manifests.push(("class label internal CR", class_cr));

        let mut format_control = valid_manifest();
        format_control.annotation_format = "visualai\u{0085}normalized/v1".to_string();
        invalid_manifests.push(("annotation_format internal control", format_control));

        let mut snapshot_trailing_lf = valid_manifest();
        snapshot_trailing_lf.snapshot_name = "Training snapshot\n".to_string();
        invalid_manifests.push(("snapshot_name trailing LF", snapshot_trailing_lf));

        let mut snapshot_leading_nbsp = valid_manifest();
        snapshot_leading_nbsp.snapshot_name = "\u{00a0}Training snapshot".to_string();
        invalid_manifests.push((
            "snapshot_name leading Unicode whitespace",
            snapshot_leading_nbsp,
        ));

        let mut label_trailing_ideographic_space = valid_manifest();
        label_trailing_ideographic_space.classes[0].label = "person\u{3000}".to_string();
        invalid_manifests.push((
            "class label trailing Unicode whitespace",
            label_trailing_ideographic_space,
        ));

        let mut project_id_control = valid_manifest();
        project_id_control.project_id = "project\r1".to_string();
        invalid_manifests.push(("stable ID internal control", project_id_control));

        for (label, manifest) in invalid_manifests {
            assert!(
                serde_json::to_string(&manifest).is_err(),
                "{label} unexpectedly serialized"
            );
        }

        let mut internal_spaces = valid_manifest();
        internal_spaces.snapshot_name = "Training snapshot".to_string();
        internal_spaces.classes[0].label = "traffic light".to_string();
        serde_json::to_string(&internal_spaces).unwrap();
    }
}
