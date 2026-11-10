-- Dataset hosting is additive. Existing projects/assets remain valid and
-- dataset imports reference verified object bytes instead of copying MinIO
-- objects. Project change events remain the compatibility path for clients.

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS owner_id UUID REFERENCES users(id);

-- Existing installations had no organization-owner column. Pick the oldest
-- active member as the compatibility owner; bootstrap-admin writes this
-- explicitly for new organizations.
UPDATE organizations AS o
SET owner_id = (
    SELECT u.id
    FROM users AS u
    WHERE u.organization_id = o.id
      AND u.deleted_at IS NULL
    ORDER BY u.created_at, u.id
    LIMIT 1
)
WHERE o.owner_id IS NULL;

CREATE TABLE IF NOT EXISTS organization_quotas (
    organization_id UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    limit_bytes BIGINT NOT NULL DEFAULT 10737418240 CHECK (limit_bytes >= 0),
    max_assets BIGINT NOT NULL DEFAULT 100000 CHECK (max_assets >= 0),
    used_bytes BIGINT NOT NULL DEFAULT 0 CHECK (used_bytes >= 0),
    used_assets BIGINT NOT NULL DEFAULT 0 CHECK (used_assets >= 0),
    reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    reserved_assets BIGINT NOT NULL DEFAULT 0 CHECK (reserved_assets >= 0),
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Physical bytes count originals, thumbnails, and issue attachments. Dataset
-- references do not duplicate those bytes; their logical image rows are still
-- counted against max_assets when captured or materialized. Soft-deleted
-- records remain counted because this release has no physical-object GC.
INSERT INTO organization_quotas (organization_id, used_bytes, used_assets)
SELECT
    o.id,
    (
        COALESCE((
            SELECT SUM(a.byte_size + COALESCE(a.thumbnail_byte_size, 0))
            FROM projects p
            JOIN assets a ON a.project_id = p.id
                AND a.status NOT IN ('uploading', 'failed')
            LEFT JOIN LATERAL (
                SELECT u.organization_id
                FROM project_members pm
                JOIN users u ON u.id = pm.user_id
                WHERE pm.project_id = p.id
                  AND pm.role = 'owner'
                  AND pm.deleted_at IS NULL
                ORDER BY pm.joined_at, pm.user_id
                LIMIT 1
            ) owner_user ON TRUE
            WHERE COALESCE(p.organization_id, owner_user.organization_id) = o.id
        ), 0)
        + COALESCE((
            SELECT SUM(ia.byte_size)
            FROM issue_attachments ia
            JOIN projects p2 ON p2.id = ia.project_id
            LEFT JOIN LATERAL (
                SELECT u2.organization_id
                FROM project_members pm2
                JOIN users u2 ON u2.id = pm2.user_id
                WHERE pm2.project_id = p2.id
                  AND pm2.role = 'owner'
                  AND pm2.deleted_at IS NULL
                ORDER BY pm2.joined_at, pm2.user_id
                LIMIT 1
            ) owner_user2 ON TRUE
            WHERE COALESCE(p2.organization_id, owner_user2.organization_id) = o.id
              AND ia.status NOT IN ('uploading', 'failed')
        ), 0)
    )::BIGINT,
    COALESCE((
        SELECT COUNT(*)
        FROM projects p3
        JOIN assets a3 ON a3.project_id = p3.id
            AND a3.status NOT IN ('uploading', 'failed')
        LEFT JOIN LATERAL (
            SELECT u3.organization_id
            FROM project_members pm3
            JOIN users u3 ON u3.id = pm3.user_id
            WHERE pm3.project_id = p3.id
              AND pm3.role = 'owner'
              AND pm3.deleted_at IS NULL
            ORDER BY pm3.joined_at, pm3.user_id
            LIMIT 1
        ) owner_user3 ON TRUE
        WHERE COALESCE(p3.organization_id, owner_user3.organization_id) = o.id
    ), 0)::BIGINT
FROM organizations o
ON CONFLICT (organization_id) DO NOTHING;

CREATE TABLE IF NOT EXISTS quota_reservations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    resource_type TEXT NOT NULL CHECK (resource_type IN (
        'asset', 'issue_attachment', 'dataset_capture', 'dataset_version',
        'dataset_reuse', 'dataset_export'
    )),
    resource_id UUID NOT NULL,
    byte_size BIGINT NOT NULL CHECK (byte_size >= 0),
    asset_count BIGINT NOT NULL DEFAULT 0 CHECK (asset_count >= 0),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    consumed_byte_size BIGINT CHECK (consumed_byte_size >= 0),
    consumed_asset_count BIGINT CHECK (consumed_asset_count >= 0),
    released_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, resource_type, resource_id)
);
CREATE INDEX IF NOT EXISTS quota_reservations_active_idx
    ON quota_reservations(organization_id, expires_at)
    WHERE consumed_at IS NULL AND released_at IS NULL;

CREATE TABLE IF NOT EXISTS datasets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id),
    owner_id UUID NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema_json JSONB NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'archived', 'deleted')),
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS datasets_org_updated_idx
    ON datasets(organization_id, updated_at DESC, id)
    WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS dataset_members (
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'manager', 'viewer')),
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (dataset_id, user_id)
);
CREATE INDEX IF NOT EXISTS dataset_members_user_idx
    ON dataset_members(user_id, dataset_id)
    WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS dataset_blobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    object_key TEXT NOT NULL,
    file_name TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    byte_size BIGINT NOT NULL CHECK (byte_size > 0),
    thumbnail_key TEXT,
    thumbnail_content_hash TEXT,
    thumbnail_mime_type TEXT,
    thumbnail_byte_size BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, content_hash, byte_size, mime_type)
);
CREATE INDEX IF NOT EXISTS dataset_blobs_object_key_idx
    ON dataset_blobs(object_key);

CREATE TABLE IF NOT EXISTS dataset_items (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    blob_id UUID NOT NULL REFERENCES dataset_blobs(id),
    client_key TEXT,
    source_client_key TEXT,
    file_name TEXT NOT NULL,
    split TEXT NOT NULL DEFAULT 'train',
    annotation_json JSONB NOT NULL DEFAULT '[]',
    annotation_hash TEXT NOT NULL DEFAULT '',
    source_project_id UUID,
    source_asset_id UUID,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (dataset_id, id)
);
CREATE UNIQUE INDEX IF NOT EXISTS dataset_items_client_key_idx
    ON dataset_items(dataset_id, client_key)
    WHERE client_key IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS dataset_items_source_asset_idx
    ON dataset_items(dataset_id, source_project_id, source_asset_id)
    WHERE source_project_id IS NOT NULL AND source_asset_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS dataset_items_dataset_idx
    ON dataset_items(dataset_id, created_at, id);

CREATE TABLE IF NOT EXISTS dataset_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    version_number BIGINT NOT NULL,
    name TEXT NOT NULL,
    manifest_json JSONB NOT NULL,
    manifest_hash TEXT NOT NULL,
    manifest_byte_size BIGINT NOT NULL CHECK (manifest_byte_size >= 0),
    item_count BIGINT NOT NULL CHECK (item_count >= 0),
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (dataset_id, version_number),
    UNIQUE (id, dataset_id)
);
CREATE INDEX IF NOT EXISTS dataset_versions_dataset_idx
    ON dataset_versions(dataset_id, version_number DESC);

CREATE TABLE IF NOT EXISTS dataset_operations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('capture', 'version', 'reuse')),
    request_hash TEXT NOT NULL DEFAULT '',
    result_json JSONB NOT NULL,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (dataset_id, operation_id)
);

-- Import/export jobs are kept durable so a process restart cannot lose a
-- queued archive.  The worker implementation is intentionally separate from
-- this migration, but uses the stable columns and helper functions below.
CREATE TABLE IF NOT EXISTS dataset_jobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('import', 'export')),
    format TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    requested_by UUID NOT NULL REFERENCES users(id),
    version_id UUID,
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    input_object_key TEXT,
    output_object_key TEXT,
    input_content_hash TEXT,
    output_content_hash TEXT,
    input_byte_size BIGINT CHECK (input_byte_size >= 0),
    output_byte_size BIGINT CHECK (output_byte_size >= 0),
    quota_reservation_id UUID REFERENCES quota_reservations(id),
    error_code TEXT,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    UNIQUE (dataset_id, operation_id)
);
CREATE INDEX IF NOT EXISTS dataset_jobs_queue_idx
    ON dataset_jobs(status, created_at, id);
CREATE INDEX IF NOT EXISTS dataset_jobs_dataset_idx
    ON dataset_jobs(dataset_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS dataset_jobs_quota_reservation_idx
    ON dataset_jobs(quota_reservation_id)
    WHERE quota_reservation_id IS NOT NULL AND status IN ('queued', 'running');

-- Job uploads reserve organization bytes before the archive is persisted. The
-- worker releases this reservation on failure; keeping the operation in the
-- reservation key makes retries and process recovery idempotent.
CREATE OR REPLACE FUNCTION reserve_dataset_job_quota(
    p_dataset_id UUID,
    p_user_id UUID,
    p_bytes BIGINT,
    p_operation_id TEXT
) RETURNS UUID
LANGUAGE plpgsql
AS $$
DECLARE
    v_organization_id UUID;
    v_reserved_bytes BIGINT;
    v_reserved_assets BIGINT;
    v_expired_bytes BIGINT;
    v_expired_assets BIGINT;
    v_limit_bytes BIGINT;
    v_max_assets BIGINT;
    v_used_bytes BIGINT;
    v_used_assets BIGINT;
    v_reservation_id UUID;
BEGIN
    IF p_bytes <= 0 THEN
        RETURN NULL;
    END IF;
    SELECT d.organization_id INTO v_organization_id
    FROM datasets d
    WHERE d.id = p_dataset_id AND d.deleted_at IS NULL;
    IF v_organization_id IS NULL THEN
        SELECT p.organization_id INTO v_organization_id
        FROM projects p
        WHERE p.id = p_dataset_id AND p.deleted_at IS NULL;
    END IF;
    IF v_organization_id IS NULL THEN
        RAISE EXCEPTION 'dataset organization not found';
    END IF;
    INSERT INTO organization_quotas (organization_id)
    VALUES (v_organization_id)
    ON CONFLICT (organization_id) DO NOTHING;
    SELECT limit_bytes, max_assets, used_bytes, used_assets,
           reserved_bytes, reserved_assets
    INTO v_limit_bytes, v_max_assets, v_used_bytes, v_used_assets,
         v_reserved_bytes, v_reserved_assets
    FROM organization_quotas
    WHERE organization_id = v_organization_id
    FOR UPDATE;
    -- Keep a reservation owned by a queued/running job until the worker can
    -- release or consume it.  The FK on dataset_jobs also closes the race
    -- between this cleanup and a job insert referencing the reservation.
    SELECT COALESCE(SUM(qr.byte_size), 0), COALESCE(SUM(qr.asset_count), 0)
    INTO v_expired_bytes, v_expired_assets
    FROM quota_reservations AS qr
    WHERE qr.organization_id = v_organization_id
      AND qr.consumed_at IS NULL AND qr.released_at IS NULL
      AND qr.expires_at <= now()
      AND NOT EXISTS (
          SELECT 1
          FROM dataset_jobs AS dj
          WHERE dj.quota_reservation_id = qr.id
            AND dj.status IN ('queued', 'running')
      );
    DELETE FROM quota_reservations AS qr
    WHERE qr.organization_id = v_organization_id
      AND qr.consumed_at IS NULL AND qr.released_at IS NULL
      AND qr.expires_at <= now()
      AND NOT EXISTS (
          SELECT 1
          FROM dataset_jobs AS dj
          WHERE dj.quota_reservation_id = qr.id
            AND dj.status IN ('queued', 'running')
      );
    v_reserved_bytes := GREATEST(0, v_reserved_bytes - v_expired_bytes);
    v_reserved_assets := GREATEST(0, v_reserved_assets - v_expired_assets);
    IF v_used_bytes + v_reserved_bytes + p_bytes > v_limit_bytes THEN
        RAISE EXCEPTION 'organization storage quota exceeded';
    END IF;
    v_reservation_id := gen_random_uuid();
    INSERT INTO quota_reservations (
        id, organization_id, resource_type, resource_id,
        byte_size, asset_count, expires_at
    ) VALUES (
        v_reservation_id, v_organization_id, 'dataset_export',
        v_reservation_id, p_bytes, 0, now() + interval '30 minutes'
    );
    UPDATE organization_quotas
    SET reserved_bytes = v_reserved_bytes + p_bytes,
        reserved_assets = v_reserved_assets,
        revision = revision + 1,
        updated_at = now()
    WHERE organization_id = v_organization_id;
    RETURN v_reservation_id;
END;
$$;

CREATE OR REPLACE FUNCTION release_dataset_job_quota(p_reservation_id UUID)
RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    v_organization_id UUID;
    v_bytes BIGINT;
    v_assets BIGINT;
BEGIN
    SELECT organization_id INTO v_organization_id
    FROM quota_reservations
    WHERE id = p_reservation_id
      AND consumed_at IS NULL AND released_at IS NULL;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    PERFORM organization_id
    FROM organization_quotas
    WHERE organization_id = v_organization_id
    FOR UPDATE;
    SELECT organization_id, byte_size, asset_count
    INTO v_organization_id, v_bytes, v_assets
    FROM quota_reservations
    WHERE id = p_reservation_id
      AND consumed_at IS NULL AND released_at IS NULL
    FOR UPDATE;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    UPDATE organization_quotas
    SET reserved_bytes = GREATEST(0, reserved_bytes - v_bytes),
        reserved_assets = GREATEST(0, reserved_assets - v_assets),
        revision = revision + 1,
        updated_at = now()
    WHERE organization_id = v_organization_id;
    UPDATE quota_reservations SET released_at = now()
    WHERE id = p_reservation_id;
END;
$$;

-- A materialized project asset may reuse verified object bytes while its
-- annotation row remains independent. The old object-key uniqueness was a
-- false restriction once a dataset is reusable across projects.
ALTER TABLE assets
    DROP CONSTRAINT IF EXISTS assets_object_key_key;
CREATE INDEX IF NOT EXISTS assets_object_key_idx ON assets(object_key);
ALTER TABLE assets
    ADD COLUMN IF NOT EXISTS dataset_id UUID REFERENCES datasets(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS dataset_version_id UUID REFERENCES dataset_versions(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS dataset_blob_id UUID REFERENCES dataset_blobs(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS assets_dataset_blob_idx
    ON assets(dataset_blob_id)
    WHERE dataset_blob_id IS NOT NULL;
