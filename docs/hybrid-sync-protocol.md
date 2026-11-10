# Hybrid synchronization protocol

## Local state

Every mutable synchronized row carries:

- `revision`: last accepted entity revision.
- `sync_state`: `local_only`, `clean`, `dirty`, `pending`, `failed`, or
  `conflict`.
- `remote_id`: stable server UUID once published.
- `updated_at`: local display and diagnostics timestamp, never the conflict
  authority.

Every local mutation and its Outbox operation are committed in the same SQLite
transaction. The Outbox operation ID is stable across retries.

## Push

1. Read due Outbox operations in creation order, maximum 500.
2. Mark the selected operations as sending without deleting them.
3. Send `POST /api/v1/sync/push` with device ID, remote project ID, and the
   operations.
4. Apply each result independently:

| Result | Local action |
| --- | --- |
| `applied` | Save remote ID/revision, mark entity clean, delete or archive Outbox row |
| `duplicate` | Treat as applied using the stored server result |
| `conflict` | Persist base/local/remote payloads, mark entity conflict |
| `rejected` | Mark failed and require user or permission change |
| `retryable` | Increment attempt count and schedule exponential backoff |

The server stores operation results before acknowledging them. Repeating an
operation ID returns the original result without repeating the mutation.
The server also stores a request fingerprint. Reusing an operation ID with a
different entity/payload is rejected, while a processing reservation older than
five minutes can be reclaimed under the project write lock after a crashed
client/server process.

The current mobile operation vocabulary is:

- `annotation/update` with `entityId` equal to the local image key and a
  payload `{ "objects": [...] }`.
- `annotation/submit` with the same image key and `baseRevision`.
- `qa_review/transition` (the alias `review/transition` is accepted) with
  `imageId`, `decision`, `note`, and `annotationRevision`.
- `task/create` with `name`, `status`, `imageIds`, and optional `assigneeId`;
  `task_item/claim` and `task_item/release` with `taskId`, `imageId`, and the
  item `baseRevision`.
- `issue/create` with an issue `clientKey`, image/annotation identity, title,
  description, severity, and status; `issue/update`/`issue/transition` carry
  the issue revision and changed fields.
- `issue_comment/comment` carries the issue identity, stable comment
  `clientKey`, and content. Replaying the same client key is idempotent.
- `folder/create`, `folder/update`, `folder/delete`, and
  `folder_member/update` carry the folder/image client identities and base
  revision. `project/update` is reserved for explicit project metadata/mode
  changes; opening or publishing a cache must not enqueue it implicitly.

Every successful or conflicting annotation/review result contains the complete
remote shape, not only a geometry array: `id`, server `imageId`, `revision`,
`status`, and `objects`. For a review result `id` is the review UUID and
`annotationId` identifies the updated annotation. A conflict also includes `baseRevision` and
`currentRevision` in `details`. This lets a mobile client normalize its local
image key to the server UUID without losing the remote version.

## Pull

1. Request `/changes?cursor=<last_cursor>&limit=500`.
2. Apply changes in ascending `sequence` order in one local transaction.
3. Never overwrite an entity that has pending local operations. Store a
   conflict instead.
4. Advance the cursor only after all changes in the page commit.
5. Continue while `hasMore` is true.

Deleting an entity is represented by a tombstone change. Clients keep the
tombstone until no supported cursor can still reference the old row.

## Bootstrap

Bootstrap is used when linking a project, recovering a missing cursor, or when
the server has compacted changes older than the client cursor.

1. Download the project snapshot and snapshot cursor. The snapshot includes
   `tasks`, `taskItems`, `qaReviews`, and `labelSchemas` alongside assets,
   annotations, issues, and members.
2. Import metadata in one SQLite transaction.
3. Preserve local dirty entities and convert overlaps to explicit conflicts.
4. Set the cursor to the snapshot cursor.
5. Run a normal push followed by pull.

## Conflict policy

- Annotation geometry conflicts are never merged silently.
- Independent Issue comments are append-only and can be merged.
- Folder membership additions are set-union; removal versus local addition is a
  conflict.
- Project/member permission conflicts always accept the server value.
- Binary assets use content hashes. Different hashes under one logical asset
  create a conflict rather than replacing cached bytes.

## Retry schedule

Retryable failures use bounded exponential backoff with jitter:

`min(5 seconds * 2^attempt, 15 minutes) + random(0..20%)`

Authentication and authorization failures do not auto-retry indefinitely.
Network loss leaves operations durable and resumes when connectivity returns.

## Account and object access

Mobile accounts use `POST /api/v1/auth/login` with a device ID and platform.
Access JWTs are short lived; `POST /api/v1/auth/refresh` rotates the opaque
refresh token. `POST /api/v1/projects/{projectId}/invitations` creates a
seven-day, single-use project invitation, and accepting it creates the first
account session when the invitee is new. A device revoke invalidates all of its
sessions.

Images and issue attachments are never returned through a public MinIO URL.
The upload-session and download-url responses contain a five-minute
`/api/v1/object-tickets/{token}` URL. The API streams the object through the
private S3 endpoint and checks expiry, direction, expected size, and one-time
upload use. Set `PUBLIC_BASE_URL` to the externally reachable HTTPS prefix
(for example `https://175.178.16.90/image-annotation`) so mobile clients never
construct an internal storage URL.

## Diagnostics

Expose the following without exposing access tokens:

- Local and remote project IDs.
- Device ID.
- Last successful push and pull.
- Current cursor.
- Pending, failed, and conflict counts.
- Oldest pending operation age.
- Last sanitized error code and request ID.
- Cache count and bytes.
