# Build: docker build -t levyks/dbsaci:0.2.0 .
# Run:   docker run --rm -p 1521:1521 -e DBSACI_DB_HOST=... levyks/dbsaci:0.2.0
#
# dbSaci has no C libraries to link against — `ring` (rustls provider) and the
# RustCrypto auth ciphers compile from vendored assembly, tokio-postgres and
# mysql_async are pure Rust, no libpq. It builds as an ordinary dynamic glibc
# binary and ships on distroless/cc (glibc + libgcc_s + CA roots + tzdata,
# ~20 MB), so the release image is that base plus the ~18 MB binary. No musl
# cross-toolchain, no `scratch` static-linking caveats around DNS.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `[[test]]` entries in Cargo.toml must resolve for the manifest to parse.
COPY tests ./tests
RUN cargo build --release --locked --bin dbsaci \
 && strip target/release/dbsaci

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/dbsaci /usr/local/bin/dbsaci
# Oracle TNS listener; health server (when DBSACI_HEALTH_ADDR is set).
EXPOSE 1521 9500
ENTRYPOINT ["/usr/local/bin/dbsaci"]
