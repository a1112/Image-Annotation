# Publishing a local project

Publishing converts a `local_only` project into `cloud_linked` or `mirrored`
without changing local logical IDs.

## Preconditions

- The local SQLite schema is at the current version.
- Every image path is readable.
- Annotation JSON passes local validation.
- No unresolved local database migration error exists.
- The user has a valid server credential.

## Procedure

1. Create the remote project and store its UUID in
   `projects.remote_id`/`remote_project_configs`.
2. Persist mode, server URL, device ID, cache policy, and automatic-sync choice.
3. For every local image:
   - Stream SHA-256 and byte size.
   - Request an upload session with local image ID as `clientKey`.
   - Upload and complete the object.
   - Save remote asset ID, object key, hash, MIME type, and byte size.
4. Queue current annotations, folders, folder memberships, Issues, comments, and
   attachments through the normal Outbox.
5. Run push until no immediately due operation remains.
6. Pull remote changes from cursor zero or bootstrap.
7. Mark the project linked only after its remote project ID and initial cursor
   are durable.

The procedure is resumable. Existing server rows are found by
`(project_id, client_key)`, so repeating a completed stage must not duplicate
entities or objects.

## Failure handling

- Failure before remote project creation leaves the project `local_only`.
- Failure after project creation keeps the remote configuration and displays
  `publishing`; retry resumes from stored remote IDs.
- A failed image upload remains `uploading` or `failed` and does not block
  metadata for unrelated images.
- Closing the application never discards Outbox rows.
- Changing to `mirrored` starts background cache hydration after metadata sync.

## Unlinking

Unlinking does not delete remote data. It requires one of:

- Convert to `local_only` after ensuring all needed objects are available
  locally.
- Remove only the local mirror while preserving the remote project.

Remote project deletion is a distinct owner-only operation with explicit
confirmation and audit logging.
