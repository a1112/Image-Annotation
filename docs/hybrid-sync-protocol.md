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

1. Download the project snapshot and snapshot cursor.
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
