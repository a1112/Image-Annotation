-- Branch-local project metadata for dataset-card grouping.  Groups are an
-- organisation layer only: images, annotations, classes and review state stay
-- in the existing worktree entries and dataset metadata rows.
ALTER TABLE data_project_worktrees
    ADD COLUMN IF NOT EXISTS project_metadata_json JSONB NOT NULL DEFAULT
        '{"schemaVersion": 1, "datasetGroups": []}'::jsonb;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'data_project_worktrees_project_metadata_object_check'
          AND conrelid = 'data_project_worktrees'::regclass
    ) THEN
        ALTER TABLE data_project_worktrees
            ADD CONSTRAINT data_project_worktrees_project_metadata_object_check
            CHECK (jsonb_typeof(project_metadata_json) = 'object');
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS data_project_worktrees_project_metadata_idx
    ON data_project_worktrees(project_id, branch_name);
