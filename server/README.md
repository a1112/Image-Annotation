# Hybrid Project Server

The service is the remote half of the hybrid project-management model. PostgreSQL
stores project metadata, members, revisions, issues, audit events, sync cursors,
and idempotency records. MinIO stores original images and generated artifacts.

## Local stack

```bash
cp .env.example .env
docker compose up --build
```

Default endpoints:

- API: `http://127.0.0.1:16423`
- MinIO S3 API: `http://127.0.0.1:16421`
- MinIO console: `http://127.0.0.1:16422`
- PostgreSQL: `postgres://image_annotation:image_annotation_dev@127.0.0.1:16420/image_annotation`

The compose stack waits for PostgreSQL and MinIO, creates the object bucket, then
starts the API. SQLx migrations are applied by the server during startup.

## Configuration

| Variable | Purpose |
| --- | --- |
| `DATABASE_URL` | PostgreSQL connection URL |
| `SERVER_ADDR` | API listen address |
| `BIND_ADDRESS` | Preferred API listen address (falls back to `SERVER_ADDR`) |
| `JWT_SECRET` | HS256 token signing secret |
| `PUBLIC_BASE_URL` | Public HTTPS prefix used for invitations and object tickets |
| `ACCESS_TTL_SECONDS` | Short-lived access JWT lifetime (default 900) |
| `REFRESH_TTL_SECONDS` | Rotating refresh session lifetime (default 2592000) |
| `S3_ENDPOINT` | S3-compatible endpoint |
| `S3_REGION` | Object-storage region |
| `S3_BUCKET` | Object bucket |
| `S3_ACCESS_KEY` | Object-storage access key |
| `S3_SECRET_KEY` | Object-storage secret key |
| `RUST_LOG` | Rust tracing filter |

The compose file also supplies standard AWS SDK environment aliases. Store
production secrets in the deployment platform's secret manager rather than in
`.env`.

## First administrator and mobile login

Run the one-shot CLI after the database is reachable (the command applies
migrations first):

```bash
BOOTSTRAP_ADMIN_EMAIL=admin@example.com \
BOOTSTRAP_ADMIN_PASSWORD='change-this-password' \
BOOTSTRAP_ADMIN_DISPLAY_NAME='Administrator' \
BOOTSTRAP_ADMIN_ORGANIZATION='Example Team' \
cargo run -- bootstrap-admin
```

Mobile clients call `POST /api/v1/auth/login` with `email`, `password`,
`deviceId`, `platform` (`ios` or `android`), and optional `deviceName`. The
access token is short lived and the opaque refresh token is rotated on every
`/auth/refresh` call. Invites are created by project managers/owners and are
single-use for seven days.

The upload-session and download-url endpoints return API object-ticket URLs,
not MinIO URLs. Configure `PUBLIC_BASE_URL` to the externally reachable
Nginx prefix before issuing tickets.

## Browser workspace and same-origin sessions

The browser workspace is served at `/app` by the API process (`WEB_DIST_DIR`
defaults to `../dist-web`). It uses separate same-origin session endpoints:

- `POST /api/v1/web/auth/login` creates rotating `HttpOnly` cookies named
  `ia_web_access` and `ia_web_refresh`.
- `GET /api/v1/web/auth/session` reads the current cookie-backed account.
- `POST /api/v1/web/auth/refresh` rotates the refresh session, and
  `POST /api/v1/web/auth/logout` revokes it.
- `POST /api/v1/web/commands/{command}` is the browser DTO command endpoint.
  The request body may be the command object or `{ "args": { ... } }`; the
  response is `{ "ok": true, "data": ... }`. Commands enforce the
  data-project ACL in PostgreSQL and do not read a desktop checkout or local
  path.

Browser mutations require the configured same-origin `Origin`. The access and
refresh credentials are never returned to browser JSON or stored in Web Storage.
`PUBLIC_BASE_URL` must include the externally visible reverse-proxy prefix when
one exists. For example, with
`PUBLIC_BASE_URL=https://host.example/image-annotation`, proxy
`/image-annotation/api/...` to the server's `/api/...` and
`/image-annotation/app/` to its `/app/`; the cookie Path and generated polling
URLs then remain inside that prefix. The server's API routes are still rooted
at `/api/...` behind the proxy.

Web image upload and content routes are authenticated and project/dataset ACL
scoped:

- `PUT /api/v1/web/datasets/{datasetId}/images/upload?fileName=...` accepts the
  image bytes and optional `branch`/`X-Object-Size`; the server verifies the
  decoded image and SHA-256 before creating a project-scoped object.
- `GET /api/v1/web/datasets/{datasetId}/images/{imageId}/content` streams an
  image from the active or requested branch.
- `GET /api/v1/web/projects/{projectId}/objects/{hash}` streams a verified
  project object. Both routes re-check current membership and do not expose S3
  URLs.

The upload path is intentionally outside the 32 MiB command body limit. The
application still bounds an image upload at 256 MiB and 100 million pixels;
deployment-level request and temporary-storage quotas should be configured as
appropriate for the host.

## Server-side image transforms

Image conversion is a durable PostgreSQL task backed by the S3-compatible object
store. Submit either:

- `POST /api/v1/projects/{projectId}/image-transforms` with a legacy
  `sourceAssetId`, or
- `POST /api/v1/data-projects/{projectId}/image-transforms` with a project
  `sourceHash` and optional canonical `datasetKey`.

The request requires a unique `operationId`. Supported output formats are
JPEG, PNG, WebP, and TIFF (the common 8-bit workflow), with optional resize,
crop, 90/180/270-degree rotation, and horizontal/vertical flips. Input/output
bytes, dimensions, decoded pixels, crop bounds, and output allocations are
bounded before the blocking image decoder runs. The original image, annotation,
and dataset metadata are never overwritten: a successful task stores a new
content-addressed output object and result metadata.

Poll `GET /api/v1/image-transforms/{taskId}`; a succeeded task includes an
authenticated `downloadUrl` at `/api/v1/image-transforms/{taskId}/download`.
`GET /api/v1/data-projects/{projectId}/image-transforms` lists durable project
history. Download re-checks the current project ACL, verifies output size
metadata, and returns a private attachment response. Running tasks are recovered
by the worker after a process restart; production runtime recovery still needs
deployment-specific verification.

## Database, object storage, and migrations

PostgreSQL is authoritative for accounts, ACLs, datasets, browser worktrees,
immutable project history, transform tasks, and result metadata. S3/MinIO is
authoritative for image and JSON object bytes; the server verifies content
hashes and keeps project-scoped object keys. Do not expose the S3 endpoint or
object-store credentials to the browser.

Startup applies the additive SQL migrations in order. The relevant data-project
and browser/transform additions are:

| Migration | Adds |
| --- | --- |
| `0005_data_projects.sql` | project ACL, dataset membership, immutable commits/refs, and project blobs |
| `0006_image_transforms.sql` | durable conversion tasks and output metadata |
| `0007_web_workspace.sql` | browser branches, worktrees, task items, merge sessions, and exports |
| `0008_project_blob_references.sql` | project blob reference/index support used by graph materialization |

These migrations are additive to the legacy project/dataset APIs. Back up the
PostgreSQL database and the configured S3/MinIO bucket together; a database-only
backup cannot restore referenced image bytes.

## Desktop connection

1. Start the stack and confirm `GET /health` succeeds.
2. Open the desktop application's collaboration login panel.
3. Set the server URL to `http://127.0.0.1:16423`.
4. Sign in with an activated account and password. Existing manually supplied
   tokens must be replaced by a new login. Sessions are kept in the native
   credential store, not project SQLite databases.
5. Publish or link a project, then run the initial synchronization.

## Persistent data

The `postgres-data` and `minio-data` named volumes survive container recreation.
Back up both volumes together to preserve metadata and image-object consistency.

## Dataset hosting

Dataset hosting is organization-scoped and uses the same PostgreSQL/MinIO
stack. `POST /api/v1/datasets/{datasetId}/capture` records verified project
images and independent annotation JSON without copying original object bytes;
`POST /api/v1/datasets/{datasetId}/versions` creates an immutable manifest, and
`POST /api/v1/datasets/{datasetId}/reuse` materializes independent project
assets/annotations from that manifest. A source project may be deleted after
capture without invalidating dataset blobs.

Archive conversion is durable and bounded by the `dataset_jobs` worker:

- `POST /api/v1/datasets/{id}/imports?format=...` (alias:
  `/jobs/import`) accepts an `application/zip` body and `X-Operation-Id`.
- `POST /api/v1/datasets/{id}/exports` (alias: `/jobs/export`) accepts
  `{versionId,format,operationId}`; poll `/api/v1/datasets/{id}/jobs` or
  `/api/v1/dataset-jobs/{jobId}` and download a completed export from the
  `/download` endpoint.

Organization quota defaults to 10 GiB/100,000 images. Original images,
thumbnails, attachments, imported blobs, version manifests, and export files
are counted; dataset capture/reuse also reserve logical image slots. Quota
reservations are row-locked and released on failed work. Soft deletion does
not release usage because this release has no physical-object garbage
collection.
