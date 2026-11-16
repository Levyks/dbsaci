# Build: docker build -t levyks/dbsaci:0.3.2 .
# Run:   docker run --rm -p 1521:1521 -e DBSACI_DB_HOST=... levyks/dbsaci:0.3.2
#
# A fully static musl binary on `scratch` (~20 MB image). dbSaci links no C
# libraries: `ring` and the RustCrypto ciphers build from vendored code,
# tokio-postgres and mysql_async are pure Rust, chrono-tz embeds the zone
# database. musl's own allocator is slow under this multi-threaded server
# (large fetches ~25% slower than glibc), so the musl build uses mimalloc
# (see Cargo.toml / src/bin/dbsaci.rs); with it the image matches or beats the
# glibc build on every bench/workload.py operation. A CA bundle is included for
# anything that reads the system trust store.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev build-base ca-certificates
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `[[test]]` entries in Cargo.toml must resolve for the manifest to parse.
COPY tests ./tests
RUN cargo build --release --locked --bin dbsaci \
 && strip target/release/dbsaci

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/target/release/dbsaci /usr/local/bin/dbsaci
USER 65532:65532
# Oracle TNS listener; health server (when DBSACI_HEALTH_ADDR is set).
EXPOSE 1521 9500
ENTRYPOINT ["/usr/local/bin/dbsaci"]
