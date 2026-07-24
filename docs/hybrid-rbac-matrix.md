# Hybrid project RBAC matrix

The server evaluates permission on every project-scoped request. The desktop UI
may hide unavailable actions, but it is not an authorization boundary.

| Capability | Owner | Manager | Annotator | Reviewer | Viewer |
| --- | --- | --- | --- | --- | --- |
| View project, images, annotations, issues | Yes | Yes | Yes | Yes | Yes |
| Download image or issue attachment | Yes | Yes | Yes | Yes | Yes |
| Edit annotations | Yes | Yes | Yes | No | No |
| Submit annotations | Yes | Yes | Yes | No | No |
| Acquire annotation lease | Yes | Yes | Yes | No | No |
| Create and comment on issues | Yes | Yes | Yes | Yes | No |
| Review annotation and transition issue | Yes | Yes | No | Yes | No |
| Upload images and manage folders | Yes | Yes | No | No | No |
| Upload issue attachments | Yes | Yes | No | Yes | No |
| Configure project mode and cache policy | Yes | Yes | No | No | No |
| Add or change members | Yes | Yes | No | No | No |
| Delete project | Yes | No | No | No | No |

## Enforcement rules

- Project creation makes the caller the `owner`.
- A project must always retain at least one owner.
- Membership changes and project deletion are audit events.
- A user without project membership receives `404` for project resources where
  revealing existence would leak information.
- Object-storage URLs are only generated after the corresponding project
  permission succeeds.
- Presigned URLs expire quickly and do not grant access to other object keys.
- Synchronization authorizes every operation independently; one rejected
  operation does not reject unrelated operations in the same batch.
- Server permissions are authoritative when local cached membership differs.
