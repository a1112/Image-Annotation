CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE organizations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE users (
    id UUID PRIMARY KEY,
    organization_id UUID REFERENCES organizations(id),
    display_name TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE projects (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID REFERENCES organizations(id),
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    mode TEXT NOT NULL CHECK (mode IN ('local_only', 'cloud_linked', 'mirrored')),
    status TEXT NOT NULL CHECK (status IN ('draft', 'active', 'archived', 'deleted')),
    label_schema_id UUID,
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE project_members (
    project_id UUID NOT NULL REFERENCES projects(id),
    user_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'manager', 'annotator', 'reviewer', 'viewer')),
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (project_id, user_id)
);

CREATE TABLE label_schemas (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    schema_json JSONB NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE assets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    client_key TEXT,
    file_name TEXT NOT NULL,
    object_key TEXT NOT NULL UNIQUE,
    thumbnail_key TEXT,
    content_hash TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    byte_size BIGINT NOT NULL,
    split TEXT NOT NULL DEFAULT 'train',
    status TEXT NOT NULL DEFAULT 'unannotated',
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);
CREATE INDEX assets_project_updated_idx ON assets(project_id, updated_at, id);
CREATE UNIQUE INDEX assets_project_client_key_idx
    ON assets(project_id, client_key) WHERE client_key IS NOT NULL;

CREATE TABLE annotations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    image_id UUID NOT NULL UNIQUE REFERENCES assets(id),
    revision BIGINT NOT NULL DEFAULT 1,
    schema_version_id UUID,
    object_json JSONB NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'draft',
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE annotation_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    annotation_id UUID NOT NULL REFERENCES annotations(id),
    revision BIGINT NOT NULL,
    object_json JSONB NOT NULL,
    operation_id TEXT NOT NULL,
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(annotation_id, revision),
    UNIQUE(operation_id)
);

CREATE TABLE tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    assignee_id UUID,
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE task_items (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_id UUID NOT NULL REFERENCES tasks(id),
    image_id UUID NOT NULL REFERENCES assets(id),
    status TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    UNIQUE(task_id, image_id)
);

CREATE TABLE qa_reviews (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    image_id UUID NOT NULL REFERENCES assets(id),
    annotation_revision BIGINT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('approved', 'rejected')),
    note TEXT NOT NULL DEFAULT '',
    reviewer_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE issues (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    client_key TEXT,
    image_id UUID NOT NULL REFERENCES assets(id),
    annotation_object_id TEXT,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    severity TEXT NOT NULL CHECK (severity IN ('blocker', 'critical', 'major', 'minor', 'suggestion')),
    status TEXT NOT NULL CHECK (status IN ('open', 'in_progress', 'resolved', 'pending_review', 'closed', 'reopened')),
    source TEXT NOT NULL,
    reporter_id UUID NOT NULL,
    assignee_id UUID,
    due_at TIMESTAMPTZ,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ
);
CREATE INDEX issues_project_status_idx ON issues(project_id, status, updated_at);
CREATE UNIQUE INDEX issues_project_client_key_idx
    ON issues(project_id, client_key) WHERE client_key IS NOT NULL;

CREATE TABLE issue_comments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    issue_id UUID NOT NULL REFERENCES issues(id),
    author_id UUID NOT NULL,
    content TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE issue_attachments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    issue_id UUID NOT NULL REFERENCES issues(id),
    object_key TEXT NOT NULL,
    file_name TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    byte_size BIGINT NOT NULL,
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE issue_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    issue_id UUID NOT NULL REFERENCES issues(id),
    event_type TEXT NOT NULL,
    before_json JSONB,
    after_json JSONB,
    actor_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE folders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    client_key TEXT,
    parent_id UUID REFERENCES folders(id),
    name TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0,
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    updated_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX folders_project_client_key_idx
    ON folders(project_id, client_key) WHERE client_key IS NOT NULL;

CREATE TABLE folder_members (
    folder_id UUID NOT NULL REFERENCES folders(id),
    image_id UUID NOT NULL REFERENCES assets(id),
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY(folder_id, image_id)
);

CREATE TABLE snapshots (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    manifest_json JSONB NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE TABLE exports (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    snapshot_id UUID NOT NULL REFERENCES snapshots(id),
    format TEXT NOT NULL,
    status TEXT NOT NULL,
    object_key TEXT,
    error_message TEXT,
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE edit_leases (
    image_id UUID PRIMARY KEY REFERENCES assets(id),
    project_id UUID NOT NULL REFERENCES projects(id),
    lease_id UUID NOT NULL UNIQUE,
    user_id UUID NOT NULL,
    device_id TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE processed_operations (
    operation_id TEXT PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id),
    result_json JSONB NOT NULL,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE change_events (
    sequence BIGSERIAL PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id),
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    revision BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX change_events_project_sequence_idx ON change_events(project_id, sequence);

CREATE TABLE audit_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id),
    user_id UUID NOT NULL,
    device_id TEXT,
    ip_address INET,
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    result TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
