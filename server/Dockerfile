FROM node:22-bookworm-slim AS web-builder

WORKDIR /web
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY src ./src
COPY scripts ./scripts
COPY index.html tsconfig*.json vite.config.ts ./
RUN npm run build:web

FROM rust:1.94.1-bookworm AS builder

WORKDIR /build
COPY server/Cargo.toml server/Cargo.lock* ./
COPY server/migrations ./migrations
COPY server/src ./src

RUN cargo install --path . --root /opt/image-annotation

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /opt/image-annotation/bin /opt/image-annotation/bin
COPY --from=web-builder /web/dist-web /opt/image-annotation/web

ENV RUST_LOG=image_annotation_server=info,tower_http=info
ENV WEB_DIST_DIR=/opt/image-annotation/web
EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=5 \
  CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["/bin/sh", "-c", "exec \"$(find /opt/image-annotation/bin -maxdepth 1 -type f | head -n 1)\""]
