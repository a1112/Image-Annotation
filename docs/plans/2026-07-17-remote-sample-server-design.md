# Remote Sample Server Design

## Goal

Provide a standalone service that lets authenticated clients remotely create and inspect
datasets, upload and manage samples, read image assets, edit annotations, run review
workflows, and safely remove or restore data.

The first deployment target is a single Windows or Linux machine on a trusted LAN. The
HTTP contract must also work behind an HTTPS reverse proxy without changing clients.

## Existing System

The repository already has:

- a Tauri desktop application;
- a loopback-only HTTP bridge bound to `127.0.0.1:17310`;
- one SQLite database per dataset project;
- project-local image, annotation, thumbnail, snapshot, import, and export directories;
- revision-based annotation saves;
- import adapters for YOLO Detect, YOLO Segmentation, Pascal VOC, COCO, and LabelMe.

The loopback bridge accepts generic `/api/invoke/*` calls and can invoke desktop-only
commands. It is not an appropriate network security boundary and remains local-only.

## Chosen Architecture

Add a standalone `image-annotation-server` Rust binary in the existing crate. It uses
Axum for HTTP routing and reuses the domain, storage, dataset import, and annotation
adapter modules.

The remote server exposes explicit `/api/v1` resources. It never exposes the local
`/api/invoke/*` dispatcher and never accepts arbitrary server filesystem paths from a
client.

The service owns a configurable data root:

```text
data-root/
  server.sqlite
  staging/
    imports/
  trash/
  workspaces/
    default/
      registry.json
      projects/
        {project-id}/
          project.json
          project.sqlite
          assets/
          annotations/
          imports/
          exports/
          snapshots/
```

Project storage remains compatible with the desktop layout. A workspace layout object
is passed to the remote service instead of relying on the source checkout path.

## Configuration

Configuration is read from command-line options with environment variable fallbacks:

- `--bind` / `IMAGE_ANNOTATION_BIND`, default `127.0.0.1:17311`;
- `--data-dir` / `IMAGE_ANNOTATION_DATA_DIR`;
- `--admin-token` / `IMAGE_ANNOTATION_ADMIN_TOKEN`;
- `--editor-token` / `IMAGE_ANNOTATION_EDITOR_TOKEN`;
- `--reader-token` / `IMAGE_ANNOTATION_READER_TOKEN`;
- `--allowed-origin` / `IMAGE_ANNOTATION_ALLOWED_ORIGINS`;
- `--max-upload-mib` / `IMAGE_ANNOTATION_MAX_UPLOAD_MIB`, default 2048.

Binding to a non-loopback address without at least one configured token is rejected.
Secrets are never written to logs or API responses.

## Authentication And Authorization

All `/api/v1` routes except `/api/v1/health` require an
`Authorization: Bearer <token>` header.

Roles are:

- `reader`: list projects and samples, inspect metadata and annotations, download assets;
- `editor`: reader permissions plus upload, annotation save, submit, task claim, and review;
- `admin`: editor permissions plus project creation, project trash, sample trash, restore,
  and server administration.

Token comparison is constant-time. Missing or invalid credentials return `401`.
Insufficient role returns `403`. Every mutation records the role and request identifier
in the audit log.

## REST API

Responses use a stable envelope:

```json
{
  "data": {},
  "requestId": "request-..."
}
```

Errors use:

```json
{
  "error": {
    "code": "revision_conflict",
    "message": "annotation revision has changed",
    "details": {}
  },
  "requestId": "request-..."
}
```

Core routes:

```text
GET    /api/v1/health
GET    /api/v1/projects
POST   /api/v1/projects
GET    /api/v1/projects/{projectId}
PATCH  /api/v1/projects/{projectId}
DELETE /api/v1/projects/{projectId}
POST   /api/v1/projects/{projectId}/restore

GET    /api/v1/projects/{projectId}/samples
GET    /api/v1/projects/{projectId}/samples/{imageId}
PATCH  /api/v1/projects/{projectId}/samples/{imageId}
DELETE /api/v1/projects/{projectId}/samples/{imageId}
POST   /api/v1/projects/{projectId}/samples/{imageId}/restore
GET    /api/v1/projects/{projectId}/samples/{imageId}/content
GET    /api/v1/projects/{projectId}/samples/{imageId}/thumbnail

GET    /api/v1/projects/{projectId}/samples/{imageId}/annotations
PUT    /api/v1/projects/{projectId}/samples/{imageId}/annotations
GET    /api/v1/projects/{projectId}/samples/{imageId}/annotations/history
POST   /api/v1/projects/{projectId}/samples/{imageId}/submit
POST   /api/v1/projects/{projectId}/samples/{imageId}/review

POST   /api/v1/projects/{projectId}/imports
GET    /api/v1/imports/{importId}
POST   /api/v1/imports/{importId}/commit
DELETE /api/v1/imports/{importId}
```

Sample listing supports `offset`, `limit`, `split`, `status`, `qaStatus`, `classId`,
`label`, and text search parameters. The response includes `items`, `offset`, `limit`,
and `total`.

Asset endpoints support `ETag`, `If-None-Match`, byte ranges, and a content disposition
derived only from the indexed file name.

## Upload And Import Transaction

Remote clients cannot select a server-local folder. A client uploads either:

- one ZIP archive containing a dataset;
- multiple image and annotation files as multipart fields.

The server streams request parts into a unique staging directory while enforcing:

- total byte limit;
- file count limit;
- per-file name length;
- allowed image, annotation, YAML, JSON, XML, TXT, and ZIP types;
- safe relative paths;
- safe ZIP extraction with no absolute paths, parent traversal, links, or device files.

Creating an import returns an analysis result and an import identifier. Nothing is
published to the project yet. The client confirms the detected format and import
options through the commit route. Commit indexes the staged data and publishes it using
the existing importer. Cancellation removes the staging directory.

Import states are `staged`, `analyzed`, `committing`, `completed`, `failed`, and
`cancelled`. A process restart can inspect and clean stale non-terminal imports.

## Sample Mutation And Trash

Project and sample deletion is recoverable:

- metadata is marked deleted in SQLite;
- files are moved under the service trash directory on the same volume;
- API list routes exclude deleted records by default;
- restore moves files back and clears deletion metadata;
- repeated delete and restore requests are idempotent.

The first version does not permanently purge through the public API. Administrators can
manage retention directly on the server until a separate retention policy is designed.

## Annotation Concurrency

Annotation reads return a revision and an `ETag`. Annotation writes require either an
`If-Match` header or a body revision.

The existing revision comparison remains authoritative:

- matching revision saves a new version and returns the new revision;
- stale revision returns HTTP `409 revision_conflict`;
- omitting a revision is allowed only for an image with no saved server revision.

Native source files are written atomically through the existing format adapters.

## Error Handling

Domain errors are mapped to HTTP status and stable codes:

- validation and malformed input: `400`;
- authentication failure: `401`;
- authorization failure: `403`;
- missing project, sample, or import: `404`;
- revision or state conflict: `409`;
- body or upload limit exceeded: `413`;
- unsupported media or annotation format: `415` or `422`;
- unexpected storage failure: `500`.

Internal paths and raw database errors are logged with the request identifier but are
not returned to clients.

## Desktop Client Integration

The desktop client gains a backend profile:

- local desktop backend;
- remote server URL and session token.

The TypeScript API transport selects Tauri commands for local mode and `/api/v1` REST
for remote mode. The token remains in process memory for the first version; it is not
stored in browser local storage. A connection dialog verifies health and role before
activating the remote profile.

Desktop-only actions such as native folder pickers and opening Tauri child windows stay
local. Remote imports upload client-selected data instead of sending local paths.

## Deployment

Deliver:

- the standalone server binary;
- `.env.example`;
- a multi-stage Dockerfile;
- `docker-compose.yml` with a persistent data volume;
- a Caddy reverse-proxy example;
- startup and API usage documentation.

The server serves HTTP. TLS termination is delegated to Caddy, Nginx, or another trusted
reverse proxy. Proxy headers are not trusted unless explicitly enabled.

## Testing

Testing is layered:

1. unit tests for configuration, token roles, path validation, error mapping, filtering,
   trash metadata, and revision conflict translation;
2. router tests using real Axum requests and temporary SQLite/project directories;
3. import integration tests using ZIP and multipart fixtures;
4. live-process smoke tests that start the server on an ephemeral port and use a real
   HTTP client for health, authentication, project creation, upload, list, asset
   download, annotation save, conflict, delete, and restore;
5. existing Rust and frontend regression suites;
6. Docker configuration validation when Docker is available.

Completion requires the live-process workflow to pass without using the Tauri runtime.
