//! Shared project-level metadata for branch-local dataset-card grouping.
//!
//! Dataset groups are deliberately separate from per-dataset schema metadata
//! and from image/annotation entries.  The value is small, deterministic JSON
//! that is embedded in each immutable commit tree and in each branch worktree.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const PROJECT_METADATA_SCHEMA_VERSION: u32 = 1;
pub const MAX_DATASET_GROUPS: usize = 10_000;
pub const MAX_GROUP_DATASET_KEYS: usize = 100_000;

fn default_schema_version() -> u32 {
    PROJECT_METADATA_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DatasetGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub dataset_keys: Vec<String>,
    #[serde(default)]
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMetadata {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub dataset_groups: Vec<DatasetGroup>,
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            schema_version: PROJECT_METADATA_SCHEMA_VERSION,
            dataset_groups: Vec::new(),
        }
    }
}

impl ProjectMetadata {
    pub fn is_empty(&self) -> bool {
        self.dataset_groups.is_empty()
    }

    pub fn canonicalize(&mut self) {
        for group in &mut self.dataset_groups {
            let mut seen = BTreeSet::new();
            group.dataset_keys.retain(|key| seen.insert(key.clone()));
        }
        self.dataset_groups.sort_by(|left, right| {
            left.position
                .cmp(&right.position)
                .then(left.id.cmp(&right.id))
        });
    }
}

pub fn parse(value: Option<&Value>) -> Result<ProjectMetadata, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(ProjectMetadata::default());
    };
    let mut metadata: ProjectMetadata = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid project metadata: {error}"))?;
    metadata.canonicalize();
    validate(&metadata, None)?;
    Ok(metadata)
}

pub fn validate(
    metadata: &ProjectMetadata,
    project_dataset_keys: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    if metadata.schema_version != PROJECT_METADATA_SCHEMA_VERSION {
        return Err(format!(
            "unsupported project metadata schema version: {}",
            metadata.schema_version
        ));
    }
    if metadata.dataset_groups.len() > MAX_DATASET_GROUPS {
        return Err("too many dataset groups".to_string());
    }
    let mut group_ids = BTreeSet::new();
    let mut member_keys = BTreeSet::new();
    for group in &metadata.dataset_groups {
        if group.id.trim().is_empty() || group.id.len() > 128 {
            return Err("dataset group id is invalid".to_string());
        }
        if group.name.trim().is_empty() || group.name.len() > 240 {
            return Err("dataset group name is invalid".to_string());
        }
        if group.position < 0 {
            return Err("dataset group position must be non-negative".to_string());
        }
        if !group_ids.insert(group.id.clone()) {
            return Err(format!("duplicate dataset group id: {}", group.id));
        }
        if group.dataset_keys.len() > MAX_GROUP_DATASET_KEYS {
            return Err("too many dataset keys in dataset group".to_string());
        }
        for key in &group.dataset_keys {
            if key.trim().is_empty() || key.len() > 256 || key.contains("::") || key.contains('\0')
            {
                return Err(format!("invalid dataset group member key: {key}"));
            }
            if !member_keys.insert(key.clone()) {
                return Err(format!("dataset belongs to more than one group: {key}"));
            }
            if let Some(project_dataset_keys) = project_dataset_keys {
                if !project_dataset_keys.contains(key) {
                    return Err(format!(
                        "dataset group references an unknown dataset: {key}"
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn to_value(metadata: &ProjectMetadata) -> Result<Value, String> {
    serde_json::to_value(metadata).map_err(|error| format!("encode project metadata: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_metadata_is_empty_and_canonical() {
        assert_eq!(parse(None).unwrap(), ProjectMetadata::default());
        let value = serde_json::json!({
            "datasetGroups": [{
                "id": "g",
                "name": "Group",
                "datasetKeys": ["b", "a", "a"],
                "position": 0
            }]
        });
        let metadata = parse(Some(&value)).unwrap();
        assert_eq!(metadata.dataset_groups[0].dataset_keys, vec!["b", "a"]);
    }

    #[test]
    fn a_dataset_can_belong_to_only_one_group() {
        let value = serde_json::json!({
            "schemaVersion": 1,
            "datasetGroups": [
                {"id": "a", "name": "A", "datasetKeys": ["dataset"]},
                {"id": "b", "name": "B", "datasetKeys": ["dataset"]}
            ]
        });
        assert!(parse(Some(&value)).is_err());
    }
}
