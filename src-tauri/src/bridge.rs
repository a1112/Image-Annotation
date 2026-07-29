use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

pub const BRIDGE_SCHEMA_VERSION: &str = "visualai.image-annotation.snapshot/v1";
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
        validate_safe_relative_path(&self.asset_root)
            .map_err(|error| format!("asset_root: {error}"))?;

        for (class_index, class) in self.classes.iter().enumerate() {
            validate_stable_id(&class.id)
                .map_err(|error| format!("classes[{class_index}].id: {error}"))?;
            validate_non_empty_string(&class.label)
                .map_err(|error| format!("classes[{class_index}].label: {error}"))?;
        }

        for (sample_index, sample) in self.samples.iter().enumerate() {
            let prefix = format!("samples[{sample_index}]");
            validate_stable_id(&sample.id).map_err(|error| format!("{prefix}.id: {error}"))?;
            validate_safe_relative_path(&sample.relative_path)
                .map_err(|error| format!("{prefix}.relative_path: {error}"))?;
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

            for (object_index, object) in sample.objects.iter().enumerate() {
                let object_prefix = format!("{prefix}.objects[{object_index}]");
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
        BridgeClass, BridgeManifest, BridgeObject, BridgePoint, BridgeSample, BridgeSplit,
        BridgeTaskType, BRIDGE_SCHEMA_VERSION,
    };

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
        internal_spaces.annotation_format = "visualai normalized/v1".to_string();
        serde_json::to_string(&internal_spaces).unwrap();
    }
}
