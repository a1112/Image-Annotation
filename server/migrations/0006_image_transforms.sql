-- Server-side image transformations are durable jobs.  The source object is
-- recorded by key and digest at enqueue time and is never updated by a job;
-- every successful transform gets a task-scoped output object key.
--
-- `project_id` is intentionally not a foreign key: this table accepts both
-- legacy `projects.id` and project-level `data_projects.id`, selected by
-- `source_scope`.  The route performs the corresponding ACL check before
-- enqueueing and again when a result is read.

CREATE TABLE IF NOT EXISTS image_transform_tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    operation_id TEXT NOT NULL,
    source_scope TEXT NOT NULL
        CHECK (source_scope IN ('project', 'data-project')),
    project_id UUID NOT NULL,
    source_asset_id UUID,
    source_dataset_id UUID,
    source_dataset_key TEXT,
    source_hash TEXT NOT NULL CHECK (source_hash ~ '^[0-9a-fA-F]{64}$'),
    source_object_key TEXT NOT NULL,
    source_mime_type TEXT NOT NULL,
    source_byte_size BIGINT NOT NULL CHECK (source_byte_size >= 0),
    source_width INTEGER,
    source_height INTEGER,
    target_format TEXT NOT NULL
        CHECK (target_format IN ('jpeg', 'png', 'webp', 'tiff')),
    requested_width INTEGER CHECK (requested_width IS NULL OR requested_width > 0),
    requested_height INTEGER CHECK (requested_height IS NULL OR requested_height > 0),
    crop_x INTEGER CHECK (crop_x IS NULL OR crop_x >= 0),
    crop_y INTEGER CHECK (crop_y IS NULL OR crop_y >= 0),
    crop_width INTEGER CHECK (crop_width IS NULL OR crop_width > 0),
    crop_height INTEGER CHECK (crop_height IS NULL OR crop_height > 0),
    rotate SMALLINT NOT NULL DEFAULT 0 CHECK (rotate IN (0, 90, 180, 270)),
    flip TEXT NOT NULL DEFAULT 'none'
        CHECK (flip IN ('none', 'horizontal', 'vertical')),
    quality SMALLINT NOT NULL DEFAULT 85 CHECK (quality BETWEEN 1 AND 100),
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    output_hash TEXT CHECK (output_hash IS NULL OR output_hash ~ '^[0-9a-fA-F]{64}$'),
    output_object_key TEXT,
    output_mime_type TEXT,
    output_byte_size BIGINT CHECK (output_byte_size IS NULL OR output_byte_size >= 0),
    output_width INTEGER CHECK (output_width IS NULL OR output_width > 0),
    output_height INTEGER CHECK (output_height IS NULL OR output_height > 0),
    error_code TEXT,
    error_message TEXT,
    requested_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    UNIQUE (requested_by, operation_id),
    CHECK (
        (source_scope = 'project' AND source_asset_id IS NOT NULL)
        OR
        (source_scope = 'data-project' AND source_asset_id IS NULL)
    ),
    CHECK (
        (crop_x IS NULL AND crop_y IS NULL AND crop_width IS NULL AND crop_height IS NULL)
        OR
        (crop_x IS NOT NULL AND crop_y IS NOT NULL AND crop_width IS NOT NULL AND crop_height IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS image_transform_tasks_queue_idx
    ON image_transform_tasks(status, created_at, id)
    WHERE status IN ('queued', 'running');

CREATE INDEX IF NOT EXISTS image_transform_tasks_project_idx
    ON image_transform_tasks(source_scope, project_id, created_at DESC, id DESC);
