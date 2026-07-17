# syntax=docker/dockerfile:1

# 1) Build the web console in a Node stage.
FROM node:20-alpine AS web
WORKDIR /web
COPY web/package.json ./
RUN npm install
COPY web/ ./
RUN npm run build

# 2) Build a static barmed against musl. rust:alpine builds musl-native, so the
#    result runs on a bare Alpine with no libc to ship.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
# The prebuilt console; BARME_SKIP_WEB_BUILD tells build.rs to embed it as-is
# instead of reaching for npm.
COPY --from=web /web/dist web/dist
ENV BARME_SKIP_WEB_BUILD=1
RUN cargo build --release -p barmed --features ui \
    && strip target/release/barmed

# 3) Runtime image: debian-slim. It ships a real shell (dash + coreutils) for
#    `docker exec` and, unlike Alpine, carries no busybox — so it sidesteps the
#    unpatched busybox wget CVE. The barmed binary is static musl, so it runs
#    here with no glibc dependency despite the Debian base.
FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/barmed /usr/local/bin/barmed
# Persist the store by mounting a volume here.
ENV BARME_DATA_DIR=/data
VOLUME /data
# native, console, cdn, s3
EXPOSE 7373 7374 7375 9000
ENTRYPOINT ["barmed"]
