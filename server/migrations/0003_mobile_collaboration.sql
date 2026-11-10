-- Mobile collaboration, account sessions, and private object tickets.
-- This migration is additive and intentionally leaves the v1/v2 migrations
-- untouched so existing Tencent Cloud volumes can be upgraded in place.

ALTER TABLE users
    ADD COLUMN IF NOT EXISTS password_hash TEXT,
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active';

CREATE TABLE IF NOT EXISTS devices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_key TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('ios', 'android', 'desktop', 'web')),
    name TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    UNIQUE(user_id, device_key)
);

CREATE INDEX IF NOT EXISTS devices_user_active_idx
    ON devices(user_id, last_seen_at DESC)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    refresh_token_hash TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS sessions_user_active_idx
    ON sessions(user_id, last_seen_at DESC)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS project_invitations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('manager', 'annotator', 'reviewer', 'viewer')),
    token_hash TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    invited_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS project_invitations_project_idx
    ON project_invitations(project_id, created_at DESC);

ALTER TABLE task_items
    ADD COLUMN IF NOT EXISTS assignee_id UUID,
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS updated_by UUID;

-- Thumbnail metadata is kept separate from the original asset metadata so a
-- mobile cache can verify a thumb ticket without assuming the original hash
-- or byte size.
ALTER TABLE assets
    ADD COLUMN IF NOT EXISTS thumbnail_content_hash TEXT,
    ADD COLUMN IF NOT EXISTS thumbnail_mime_type TEXT,
    ADD COLUMN IF NOT EXISTS thumbnail_byte_size BIGINT;

CREATE INDEX IF NOT EXISTS task_items_assignee_idx
    ON task_items(task_id, assignee_id, status)
    WHERE deleted_at IS NULL;

ALTER TABLE qa_reviews
    ADD COLUMN IF NOT EXISTS operation_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS qa_reviews_operation_id_idx
    ON qa_reviews(operation_id)
    WHERE operation_id IS NOT NULL;

ALTER TABLE issue_comments
    ADD COLUMN IF NOT EXISTS client_key TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS issue_comments_client_key_idx
    ON issue_comments(issue_id, client_key)
    WHERE client_key IS NOT NULL;

ALTER TABLE processed_operations
    ADD COLUMN IF NOT EXISTS state TEXT NOT NULL DEFAULT 'completed',
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS completed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS request_hash TEXT;

CREATE TABLE IF NOT EXISTS object_tickets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash TEXT NOT NULL UNIQUE,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    asset_id UUID REFERENCES assets(id) ON DELETE CASCADE,
    attachment_id UUID REFERENCES issue_attachments(id) ON DELETE CASCADE,
    object_key TEXT NOT NULL,
    variant TEXT NOT NULL DEFAULT 'original',
    direction TEXT NOT NULL CHECK (direction IN ('get', 'put')),
    file_name TEXT NOT NULL DEFAULT '',
    mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    content_hash TEXT,
    expected_size BIGINT,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((asset_id IS NOT NULL) OR (attachment_id IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS object_tickets_expiry_idx
    ON object_tickets(expires_at)
    WHERE used_at IS NULL;
