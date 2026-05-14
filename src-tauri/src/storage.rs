use crate::project_fs::ProjectManifest;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct StoredImage {
    pub id: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub split: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredClass {
    pub id: u32,
    pub label: String,
    pub color: String,
}

pub fn initialize_project_database(path: &Path) -> Result<(), String> {
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source_dataset_key TEXT NOT NULL,
                format TEXT NOT NULL,
                root_path TEXT NOT NULL DEFAULT '',
                class_count INTEGER NOT NULL DEFAULT 0,
                image_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS images (
                id TEXT PRIMARY KEY,
                file_name TEXT NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                split TEXT NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS classes (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL,
                color TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS annotations (
                id TEXT PRIMARY KEY,
                image_id TEXT NOT NULL,
                object_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|err| err.to_string())?;
    for statement in [
        "ALTER TABLE projects ADD COLUMN root_path TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE projects ADD COLUMN class_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE projects ADD COLUMN image_count INTEGER NOT NULL DEFAULT 0",
    ] {
        let _ = connection.execute(statement, []);
    }
    Ok(())
}

pub fn upsert_project_index(
    path: &Path,
    manifest: &ProjectManifest,
    images: &[StoredImage],
    classes: &[StoredClass],
) -> Result<(), String> {
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    initialize_project_database(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            r#"
            INSERT INTO projects (id, name, source_dataset_key, format, root_path, class_count, image_count, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
              name = excluded.name,
              source_dataset_key = excluded.source_dataset_key,
              format = excluded.format,
              root_path = excluded.root_path,
              class_count = excluded.class_count,
              image_count = excluded.image_count,
              created_at = excluded.created_at
            "#,
            params![
                manifest.id,
                manifest.name,
                manifest.source_dataset_key,
                manifest.format,
                manifest.root_path,
                manifest.class_count,
                manifest.image_count,
                manifest.created_at,
            ],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM images", [])
        .map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM classes", [])
        .map_err(|err| err.to_string())?;

    for image in images {
        transaction
            .execute(
                "INSERT INTO images (id, file_name, width, height, split, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![image.id, image.file_name, image.width, image.height, image.split, image.status],
            )
            .map_err(|err| err.to_string())?;
    }

    for class in classes {
        transaction
            .execute(
                "INSERT INTO classes (id, label, color) VALUES (?1, ?2, ?3)",
                params![class.id, class.label, class.color],
            )
            .map_err(|err| err.to_string())?;
    }

    transaction.commit().map_err(|err| err.to_string())
}

pub fn read_project_manifest(path: &Path) -> Result<Option<ProjectManifest>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .query_row(
            "SELECT id, name, source_dataset_key, format, root_path, class_count, image_count, created_at FROM projects LIMIT 1",
            [],
            |row| {
                Ok(ProjectManifest {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    source_dataset_key: row.get(2)?,
                    format: row.get(3)?,
                    root_path: row.get(4)?,
                    class_count: row.get::<_, u32>(5)?,
                    image_count: row.get::<_, u32>(6)?,
                    created_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
}

pub fn read_images(path: &Path, split: Option<&str>) -> Result<Vec<StoredImage>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let sql = if split.is_some() {
        "SELECT id, file_name, width, height, split, status FROM images WHERE split = ?1 ORDER BY file_name"
    } else {
        "SELECT id, file_name, width, height, split, status FROM images ORDER BY file_name"
    };
    let mut statement = connection.prepare(sql).map_err(|err| err.to_string())?;
    let rows = if let Some(split) = split {
        statement
            .query_map(params![split], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    } else {
        statement
            .query_map([], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    };
    Ok(rows)
}

pub fn read_classes(path: &Path) -> Result<Vec<StoredClass>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, label, color FROM classes ORDER BY id")
        .map_err(|err| err.to_string())?;
    let classes = statement
        .query_map([], |row| {
            Ok(StoredClass {
                id: row.get(0)?,
                label: row.get(1)?,
                color: row.get(2)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(classes)
}

fn stored_image_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredImage> {
    Ok(StoredImage {
        id: row.get(0)?,
        file_name: row.get(1)?,
        width: row.get(2)?,
        height: row.get(3)?,
        split: row.get(4)?,
        status: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_project_database_schema() {
        let path = std::env::temp_dir().join("image_annotation_schema_test.sqlite");
        let _ = std::fs::remove_file(&path);

        initialize_project_database(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        let count: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('projects', 'images', 'classes', 'annotations')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 4);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn writes_and_reads_project_image_and_class_index() {
        let path = std::env::temp_dir().join("image_annotation_index_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        let manifest = ProjectManifest {
            id: "fixture".to_string(),
            name: "Fixture".to_string(),
            source_dataset_key: "fixture".to_string(),
            format: "yolo-detect".to_string(),
            root_path: "F:/fixture".to_string(),
            created_at: "1".to_string(),
            class_count: 2,
            image_count: 1,
        };
        let images = vec![StoredImage {
            id: "0001".to_string(),
            file_name: "0001.png".to_string(),
            width: 4,
            height: 3,
            split: "train".to_string(),
            status: "已标注".to_string(),
        }];
        let classes = vec![
            StoredClass {
                id: 0,
                label: "person".to_string(),
                color: "#1fa7ff".to_string(),
            },
            StoredClass {
                id: 1,
                label: "car".to_string(),
                color: "#cc54d8".to_string(),
            },
        ];

        upsert_project_index(&path, &manifest, &images, &classes).unwrap();

        assert_eq!(read_project_manifest(&path).unwrap().unwrap().id, "fixture");
        assert_eq!(read_images(&path, None).unwrap(), images);
        assert_eq!(read_classes(&path).unwrap(), classes);
        let _ = std::fs::remove_file(path);
    }
}
