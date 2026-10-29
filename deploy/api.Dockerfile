# The API server (toshi-server). Build from the repository root:
#   docker build -f deploy/api.Dockerfile -t toshi-api .

FROM rust:1-bookworm AS build
# protoc: the Lightning client generates its code from protobuf files at build time.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p escrow-api --bin toshi-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin toshi
COPY --from=build /src/target/release/toshi-server /usr/local/bin/toshi-server
COPY deploy/api-entrypoint.sh /usr/local/bin/api-entrypoint.sh
RUN chmod 0755 /usr/local/bin/api-entrypoint.sh
USER toshi
# Render's default port. The server must listen on every interface, not only loopback.
ENV BIND=0.0.0.0:10000
EXPOSE 10000
ENTRYPOINT ["/usr/local/bin/api-entrypoint.sh"]
