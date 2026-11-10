-- Server-backed web workspace for data projects.  The 0005 graph remains the
-- immutable sync authority; these tables hold the branch-local browser draft
-- and user interaction state without depending on a native SQLite checkout.

-- Do not add a global UNIQUE constraint here.  Older 0005 graph imports may
-- legitimately contain the same hosted Dataset UUID in more than one graph;
-- making the migration fail would hide those projects.  New web assignments
-- enforce the one-owner rule transactionally in the command handler while
-- retaining a non-unique lookup index for the identity resolver.
CREATE INDEX IF NOT EXISTS data_project_dataset_single_owner_idx
    ON data_project_datasets(dataset_id)
    WHERE dataset_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS data_project_branches (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    branch_name TEXT NOT NULL,
    protected BOOLEAN NOT NULL DEFAULT FALSE,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (project_id, branch_name),
    CHECK (char_length(branch_name) BETWEEN 1 AND 128)
);

CREATE INDEX IF NOT EXISTS data_project_branches_active_idx
    ON data_project_branches(project_id, branch_name)
    WHERE deleted_at IS NULL;

-- Existing projects created before this migration may have no ref row.  A
-- protected empty main is the deterministic web starting point.
INSERT INTO data_project_refs (project_id, ref_name, commit_id, revision, updated_by)
SELECT p.id, 'main', NULL, GREATEST(1, p.revision), p.updated_by
FROM data_projects p
WHERE p.deleted_at IS NULL
  AND NOT EXISTS (
      SELECT 1 FROM data_project_refs r
      WHERE r.project_id = p.id AND r.ref_name = 'main'
  );

INSERT INTO data_project_branches (project_id, branch_name, protected, created_by)
SELECT r.project_id, r.ref_name, (r.ref_name = 'main'), r.updated_by
FROM data_project_refs r
ON CONFLICT (project_id, branch_name) DO NOTHING;

CREATE TABLE IF NOT EXISTS data_project_worktrees (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    branch_name TEXT NOT NULL,
    base_commit_id TEXT,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by UUID NOT NULL REFERENCES users(id),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, branch_name),
    FOREIGN KEY (project_id, branch_name)
        REFERENCES data_project_branches(project_id, branch_name)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS data_project_worktree_metadata (
    project_id UUID NOT NULL,
    branch_name TEXT NOT NULL,
    dataset_key TEXT NOT NULL,
    dataset_id UUID,
    metadata_hash TEXT,
    metadata_json JSONB NOT NULL DEFAULT '{}',
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by UUID NOT NULL REFERENCES users(id),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, branch_name, dataset_key),
    FOREIGN KEY (project_id, branch_name)
        REFERENCES data_project_branches(project_id, branch_name)
        ON DELETE CASCADE,
    -- Dataset class schemas are accepted by the existing dataset API as
    -- either an object or a top-level array; preserve both representations
    -- in the browser worktree instead of rejecting array-style class lists.
    CHECK (jsonb_typeof(metadata_json) IN ('object', 'array'))
);

CREATE TABLE IF NOT EXISTS data_project_worktree_entries (
    project_id UUID NOT NULL,
    branch_name TEXT NOT NULL,
    dataset_key TEXT NOT NULL,
    dataset_id UUID,
    image_id TEXT NOT NULL,
    file_name TEXT NOT NULL,
    image_hash TEXT NOT NULL,
    image_byte_size BIGINT,
    image_mime_type TEXT,
    annotation_hash TEXT NOT NULL,
    annotation_json JSONB NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'raw',
    qa_status TEXT NOT NULL DEFAULT 'pending',
    split TEXT NOT NULL DEFAULT 'train',
    width INTEGER NOT NULL DEFAULT 0,
    height INTEGER NOT NULL DEFAULT 0,
    review_note TEXT,
    tags_json JSONB NOT NULL DEFAULT '[]',
    source_item_id UUID,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by UUID NOT NULL REFERENCES users(id),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, branch_name, dataset_key, image_id),
    FOREIGN KEY (project_id, branch_name)
        REFERENCES data_project_branches(project_id, branch_name)
        ON DELETE CASCADE,
    CHECK (jsonb_typeof(annotation_json) IN ('array', 'object')),
    CHECK (jsonb_typeof(tags_json) = 'array')
);

CREATE INDEX IF NOT EXISTS data_project_worktree_entries_page_idx
    ON data_project_worktree_entries(project_id, branch_name, dataset_key, updated_at, image_id);
CREATE INDEX IF NOT EXISTS data_project_worktree_entries_status_idx
    ON data_project_worktree_entries(project_id, branch_name, status, qa_status);

CREATE TABLE IF NOT EXISTS data_project_active_branches (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    branch_name TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, user_id),
    FOREIGN KEY (project_id, branch_name)
        REFERENCES data_project_branches(project_id, branch_name)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS data_project_merge_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    target_branch TEXT NOT NULL,
    source_branch TEXT NOT NULL,
    base_commit_id TEXT,
    ours_commit_id TEXT,
    theirs_commit_id TEXT,
    expected_target_revision BIGINT NOT NULL,
    message TEXT NOT NULL,
    candidate_tree_json JSONB NOT NULL,
    conflicts_json JSONB NOT NULL DEFAULT '[]',
    resolutions_json JSONB NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'resolved', 'aborted', 'expired')),
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '24 hours'),
    CHECK (jsonb_typeof(candidate_tree_json) = 'object'),
    CHECK (jsonb_typeof(conflicts_json) = 'array'),
    CHECK (jsonb_typeof(resolutions_json) = 'object')
);

CREATE INDEX IF NOT EXISTS data_project_merge_sessions_user_idx
    ON data_project_merge_sessions(project_id, created_by, updated_at DESC);

CREATE TABLE IF NOT EXISTS data_project_tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'unassigned',
    assignee_id UUID REFERENCES users(id),
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL REFERENCES users(id),
    updated_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS data_project_task_items (
    task_id UUID NOT NULL REFERENCES data_project_tasks(id) ON DELETE CASCADE,
    dataset_key TEXT NOT NULL,
    image_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'unassigned',
    assignee_id UUID REFERENCES users(id),
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, dataset_key, image_id)
);

CREATE TABLE IF NOT EXISTS data_project_exports (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    branch_name TEXT NOT NULL,
    format TEXT NOT NULL DEFAULT 'manifest-json',
    status TEXT NOT NULL DEFAULT 'succeeded'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    manifest_json JSONB NOT NULL,
    object_key TEXT,
    error_message TEXT,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS data_project_exports_project_idx
    ON data_project_exports(project_id, created_at DESC);
