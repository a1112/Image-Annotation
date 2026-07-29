use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

pub const BRIDGE_SCHEMA_VERSION: &str = "visualai.image-annotation.snapshot/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    pub created_at: String,
    pub task_type: BridgeTaskType,
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    pub annotation_format: String,
    #[serde(deserialize_with = "deserialize_safe_relative_path")]
    pub asset_root: String,
    pub classes: Vec<BridgeClass>,
    pub samples: Vec<BridgeSample>,
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
    if !value.is_empty() && value.trim() == value {
        Ok(value)
    } else {
        Err(D::Error::custom(
            "value must be non-empty and have no surrounding whitespace",
        ))
    }
}

fn deserialize_stable_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = deserialize_non_empty_string(deserializer)?;
    if value
        .chars()
        .all(|character| !character.is_control() && character != '/' && character != '\\')
    {
        Ok(value)
    } else {
        Err(D::Error::custom(
            "stable ID must not contain control characters or path separators",
        ))
    }
}

fn deserialize_optional_stable_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value {
        Some(value)
            if !value.is_empty()
                && value.trim() == value
                && value.chars().all(|character| {
                    !character.is_control() && character != '/' && character != '\\'
                }) =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(D::Error::custom(
            "revision must be a stable ID when present",
        )),
        None => Ok(None),
    }
}

fn deserialize_safe_relative_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
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
        Ok(value)
    } else {
        Err(D::Error::custom(
            "path must be a normalized, portable relative path",
        ))
    }
}

fn deserialize_positive_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if value > 0 {
        Ok(value)
    } else {
        Err(D::Error::custom("value must be greater than zero"))
    }
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value > 0 {
        Ok(value)
    } else {
        Err(D::Error::custom("value must be greater than zero"))
    }
}

fn deserialize_sha256<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() == 64
        && value
            .bytes()
            .all(|character| character.is_ascii_digit() || (b'a'..=b'f').contains(&character))
    {
        Ok(value)
    } else {
        Err(D::Error::custom(
            "sha256 must contain exactly 64 lowercase hexadecimal characters",
        ))
    }
}

fn deserialize_non_negative_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(D::Error::custom(
            "value must be finite and greater than or equal to zero",
        ))
    }
}

fn deserialize_positive_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(D::Error::custom(
            "value must be finite and greater than zero",
        ))
    }
}

fn deserialize_polygon_points<'de, D>(deserializer: D) -> Result<Vec<BridgePoint>, D::Error>
where
    D: Deserializer<'de>,
{
    let points = Vec::<BridgePoint>::deserialize(deserializer)?;
    if points.len() >= 3 {
        Ok(points)
    } else {
        Err(D::Error::custom(
            "polygon must contain at least three points",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{BridgeManifest, BRIDGE_SCHEMA_VERSION};

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
        let invalid = include_str!("../../docs/protocol/fixtures/detection.json").replacen(
            "{",
            r#"{"unexpected":true,"#,
            1,
        );
        assert!(serde_json::from_str::<BridgeManifest>(&invalid).is_err());
    }

    #[test]
    fn bridge_manifest_rejects_unsafe_paths_and_invalid_integrity_fields() {
        let fixture = include_str!("../../docs/protocol/fixtures/detection.json");

        for invalid in [
            fixture.replace("images/street.jpg", "../street.jpg"),
            fixture.replace(r#""width": 640"#, r#""width": 0"#),
            fixture.replace(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "ABCDEF",
            ),
        ] {
            assert!(serde_json::from_str::<BridgeManifest>(&invalid).is_err());
        }
    }

    #[test]
    fn bridge_object_variants_reject_unknown_fields() {
        let invalid = include_str!("../../docs/protocol/fixtures/detection.json").replace(
            r#""type": "bbox","#,
            r#""type": "bbox", "unexpected": true,"#,
        );
        assert!(serde_json::from_str::<BridgeManifest>(&invalid).is_err());
    }

    #[test]
    fn bridge_manifest_rejects_a_different_schema_version() {
        let invalid = include_str!("../../docs/protocol/fixtures/detection.json").replace(
            BRIDGE_SCHEMA_VERSION,
            "visualai.image-annotation.snapshot/v2",
        );
        assert!(serde_json::from_str::<BridgeManifest>(&invalid).is_err());
    }
}
