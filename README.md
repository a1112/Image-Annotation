# Image Annotation

Image Annotation is a Tauri desktop annotation workspace with an optional standalone,
authenticated remote sample server.

## Development

```powershell
npm ci
npm run dev
```

Run the desktop application with `npm run tauri dev`. Run frontend checks with
`npm test -- --run` and `npm run build`.

## Remote Server

The standalone server manages projects, samples, image assets, annotations, imports,
review state, and recoverable deletion through `/api/v1`.

See [docs/remote-server.md](docs/remote-server.md) for native and Docker startup,
token configuration, HTTPS proxying, backup, and API examples.
