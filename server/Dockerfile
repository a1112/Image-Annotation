FROM rust:1.88-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY migrations ./migrations
COPY src ./src

RUN cargo install --path . --root /opt/image-annotation

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /opt/image-annotation/bin /opt/image-annotation/bin

ENV RUST_LOG=image_annotation_server=info,tower_http=info
EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=5 \
  CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["/bin/sh", "-c", "exec \"$(find /opt/image-annotation/bin -maxdepth 1 -type f | head -n 1)\""]
