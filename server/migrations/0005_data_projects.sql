-- Cloud data projects sit above the existing dataset repository.  This
-- migration is deliberately additive: the legacy `projects`, `assets`,
-- `datasets`, and change-feed tables remain untouched.

CREATE TABLE IF NOT EXISTS data_projects (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID REFERENCES organizations(id) ON DELETE CASCADE,
    owner_id UUID NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'archived', 'deleted')),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_by UUID NOT NULL REFERENCES users(id),
    updated_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS data_projects_org_updated_idx
    ON data_projects(organization_id, updated_at DESC, id)
    WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS data_project_members (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'editor', 'viewer')),
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (project_id, user_id)
);

CREATE INDEX IF NOT EXISTS data_project_members_user_idx
    ON data_project_members(user_id, project_id)
    WHERE deleted_at IS NULL;

-- A membership is a stable dataset key plus an optional server Dataset UUID.
-- The key is intentionally not a foreign key: a local client may sync a
-- dataset before its cloud Dataset row exists, and clone must retain the
-- complete project graph rather than silently dropping that membership.
CREATE TABLE IF NOT EXISTS data_project_datasets (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    dataset_key TEXT NOT NULL,
    dataset_id UUID REFERENCES datasets(id) ON DELETE SET NULL,
    metadata_json JSONB NOT NULL DEFAULT '{}',
    position INTEGER NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, dataset_key)
);

CREATE INDEX IF NOT EXISTS data_project_datasets_dataset_idx
    ON data_project_datasets(dataset_id)
    WHERE dataset_id IS NOT NULL;

-- Blobs are scoped by project in both PostgreSQL and the object-store key.
-- The same hash in two projects is intentionally not a cross-project read
-- grant.  `object_key` is server generated and never accepted from clients.
CREATE TABLE IF NOT EXISTS data_project_blobs (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL CHECK (content_hash ~ '^[0-9a-fA-F]{64}$'),
    byte_size BIGINT NOT NULL CHECK (byte_size >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('image', 'annotation', 'dataset-metadata', 'project-metadata', 'other')),
    mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    object_key TEXT NOT NULL UNIQUE,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, content_hash, byte_size)
);

CREATE INDEX IF NOT EXISTS data_project_blobs_hash_idx
    ON data_project_blobs(project_id, content_hash);

-- Commit IDs are opaque strings so existing local UUID histories can be
-- imported without rewriting them.  The immutable tree contains structured
-- entries and hash references; it is not a JSON-only upload shell.
CREATE TABLE IF NOT EXISTS data_project_commits (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    commit_id TEXT NOT NULL,
    parent_ids_json JSONB NOT NULL DEFAULT '[]',
    message TEXT NOT NULL,
    tree_json JSONB NOT NULL,
    metadata_json JSONB NOT NULL DEFAULT '{}',
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, commit_id),
    CHECK (jsonb_typeof(parent_ids_json) = 'array'),
    CHECK (jsonb_typeof(tree_json) = 'object'),
    CHECK (jsonb_typeof(metadata_json) = 'object')
);

CREATE INDEX IF NOT EXISTS data_project_commits_created_idx
    ON data_project_commits(project_id, created_at, commit_id);

CREATE TABLE IF NOT EXISTS data_project_refs (
    project_id UUID NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
    ref_name TEXT NOT NULL,
    commit_id TEXT,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by UUID NOT NULL REFERENCES users(id),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, ref_name)
);

CREATE INDEX IF NOT EXISTS data_project_refs_commit_idx
    ON data_project_refs(project_id, commit_id);
