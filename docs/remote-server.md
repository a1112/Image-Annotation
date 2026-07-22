# Remote Sample Server

## Scope

`image-annotation-server` is the standalone backend for remotely managed annotation
projects. It uses one configured data root, SQLite metadata, project-local assets, and
Bearer tokens with `reader`, `editor`, and `admin` roles.

The server provides HTTP. Terminate TLS in Caddy, Nginx, or another trusted reverse
proxy before exposing it outside a trusted network.

## Roles

| Role | Access |
| --- | --- |
| `reader` | List projects and samples; read assets, annotations, and import state |
| `editor` | Reader access plus uploads, annotation saves, submit, and review |
| `admin` | Editor access plus project and sample create/delete/restore operations |

Tokens must be at least 32 bytes and must be distinct. The server never returns tokens.
The desktop client keeps its active remote token in process memory only.

Generate a token in PowerShell:

```powershell
$bytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
[Convert]::ToHexString($bytes).ToLowerInvariant()
```

## Windows Native Startup

Build the server binary:

```powershell
$env:CARGO_TARGET_DIR = 'L:\codex_build\image_annotation_remote_target'
cargo build --locked --release --manifest-path src-tauri\Cargo.toml --bin image-annotation-server
```

Configure and start it:

```powershell
$env:IMAGE_ANNOTATION_BIND = '0.0.0.0:17311'
$env:IMAGE_ANNOTATION_DATA_DIR = 'D:\image-annotation-server-data'
$env:IMAGE_ANNOTATION_ADMIN_TOKEN = '<generated-admin-token>'
$env:IMAGE_ANNOTATION_EDITOR_TOKEN = '<generated-editor-token>'
$env:IMAGE_ANNOTATION_READER_TOKEN = '<generated-reader-token>'
$env:IMAGE_ANNOTATION_ALLOWED_ORIGINS = 'https://annotation.example.com'
L:\codex_build\image_annotation_remote_target\release\image-annotation-server.exe
```

Non-loopback binding requires at least one token. Configure only roles that should be
available. `IMAGE_ANNOTATION_ALLOWED_ORIGINS` is a comma-separated list of exact browser
origins; `*` is rejected.

## Docker Compose

Create the runtime environment without committing it:

```powershell
Copy-Item .env.server.example .env.server
```

Fill `.env.server` with generated tokens and the exact frontend origin, then run:

```powershell
docker compose --env-file .env.server -f docker-compose.server.yml up --build -d
docker compose --env-file .env.server -f docker-compose.server.yml ps
```

The container runs as UID/GID `10001`, uses a read-only root filesystem, and persists
all mutable state in the `server-data` volume mounted at `/data`.

## HTTPS With Caddy

Use `deploy/Caddyfile.example` in a Caddy service on the same Docker network. Set
`IMAGE_ANNOTATION_DOMAIN` to the public hostname and point DNS at the proxy. Do not
publish port `17311` publicly when Caddy is the only intended entry point.

Configure the desktop remote profile with the HTTPS origin, for example
`https://annotation.example.com`, and one role token.

## Health And API Checks

Health is public and contains no paths or secrets:

```powershell
Invoke-RestMethod http://127.0.0.1:17311/api/v1/health
```

Verify a token and its effective role:

```powershell
$headers = @{ Authorization = 'Bearer <token>' }
Invoke-RestMethod http://127.0.0.1:17311/api/v1/session -Headers $headers
Invoke-RestMethod http://127.0.0.1:17311/api/v1/projects -Headers $headers
```

Create a project with an admin token:

```powershell
$body = @{ name = 'Remote BBox'; datasetType = 'yolo-detect' } | ConvertTo-Json
Invoke-RestMethod http://127.0.0.1:17311/api/v1/projects `
  -Method Post -Headers $headers -ContentType 'application/json' -Body $body
```

Every JSON response includes a server-generated `requestId`. Use it to correlate client
errors with server logs. A `409 revision_conflict` means the client must reload the
annotation state before saving again.

## Data Layout And Backup

The configured data root contains:

```text
server.sqlite
projects/<project-id>/project.json
projects/<project-id>/project.sqlite
projects/<project-id>/assets/
projects/<project-id>/annotations/
imports/
trash/projects/
```

Back up the entire data root as one unit. Stop writes first so SQLite files and assets
represent the same point in time:

```powershell
docker compose --env-file .env.server -f docker-compose.server.yml stop
docker run --rm -v image-annotation-server-data:/data -v ${PWD}:/backup `
  alpine tar czf /backup/image-annotation-data.tgz -C /data .
docker compose --env-file .env.server -f docker-compose.server.yml start
```

To restore, stop the service, replace the complete volume contents from one backup, and
start the service. Do not restore only `server.sqlite` or only one project directory.

## Operations

- Keep `.env.server` outside version control and restrict its filesystem permissions.
- Rotate a token by restarting with a new value, then reconnect clients.
- Monitor health, container restarts, disk capacity, and `5xx` responses.
- Set `IMAGE_ANNOTATION_MAX_UPLOAD_MIB` to the largest accepted compressed and extracted
  import size that the host can safely store.
- Preserve the data-root lock: run only one server process against a given data root.
