-- 0005 made object_key globally unique.  A hosted source dataset may be
-- referenced by more than one legacy graph, though, and bootstrap registers
-- that source object in each project without copying its bytes.  Scope the
-- uniqueness to a project so each graph retains its own ACL row while the
-- S3 key can safely point at the shared immutable source object.
ALTER TABLE data_project_blobs
    DROP CONSTRAINT IF EXISTS data_project_blobs_object_key_key;
CREATE UNIQUE INDEX IF NOT EXISTS data_project_blobs_project_object_key_idx
    ON data_project_blobs(project_id, object_key);
