# pool-server image. The server needs no GPU; miners run natively on GPU machines.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p pool-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/pool-server /usr/local/bin/pool-server
COPY config /app/config
WORKDIR /app
USER 10001
ENV POOL_BIND=0.0.0.0:8080 \
    POOL_CATALOG=/app/config/models.json
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --start-interval=1s CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["pool-server"]
