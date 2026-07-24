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

- API: `http://127.0.0.1:8080`
- MinIO S3 API: `http://127.0.0.1:9000`
- MinIO console: `http://127.0.0.1:9001`
- PostgreSQL: `postgres://image_annotation:image_annotation_dev@127.0.0.1:5432/image_annotation`

The compose stack waits for PostgreSQL and MinIO, creates the object bucket, then
starts the API. SQLx migrations are applied by the server during startup.

## Configuration

| Variable | Purpose |
| --- | --- |
| `DATABASE_URL` | PostgreSQL connection URL |
| `SERVER_ADDR` | API listen address |
| `JWT_SECRET` | HS256 token signing secret |
| `S3_ENDPOINT` | S3-compatible endpoint |
| `S3_REGION` | Object-storage region |
| `S3_BUCKET` | Object bucket |
| `S3_ACCESS_KEY` | Object-storage access key |
| `S3_SECRET_KEY` | Object-storage secret key |
| `RUST_LOG` | Rust tracing filter |

The compose file also supplies standard AWS SDK environment aliases. Store
production secrets in the deployment platform's secret manager rather than in
`.env`.

## Desktop connection

1. Start the stack and confirm `GET /health` succeeds.
2. Open the desktop application's synchronization settings.
3. Set the server URL to `http://127.0.0.1:8080`.
4. Store the access token through the desktop credential command. Tokens are kept
   in the macOS Keychain and are not written into project SQLite databases.
5. Publish or link a project, then run the initial synchronization.

## Persistent data

The `postgres-data` and `minio-data` named volumes survive container recreation.
Back up both volumes together to preserve metadata and image-object consistency.
