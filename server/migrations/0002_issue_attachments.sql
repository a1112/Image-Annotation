CREATE TABLE issue_attachments (
    id UUID PRIMARY KEY,
    issue_id UUID NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    client_key TEXT,
    file_name TEXT NOT NULL,
    object_key TEXT NOT NULL UNIQUE,
    content_hash TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    byte_size BIGINT NOT NULL CHECK (byte_size > 0),
    status TEXT NOT NULL DEFAULT 'uploading'
        CHECK (status IN ('uploading', 'ready', 'failed')),
    revision BIGINT NOT NULL DEFAULT 1,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX issue_attachments_project_client_key_unique
    ON issue_attachments(project_id, client_key)
    WHERE client_key IS NOT NULL;

CREATE INDEX issue_attachments_issue_created_index
    ON issue_attachments(issue_id, created_at)
    WHERE deleted_at IS NULL;
