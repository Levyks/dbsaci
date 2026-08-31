# Build: docker build -t levyks/pgsaci:0.0.1 .
# Run:   docker run --rm -p 1521:1521 -e PGSACI_PG_HOST=... levyks/pgsaci:0.0.1
#
# pgSaci has no C dependencies (tokio-postgres is pure Rust; RustCrypto for the
# auth ciphers; no libpq, no OpenSSL), so it links fully static against musl and
# ships on `scratch` — the image is just the ~10 MB binary.
FROM rust:1-bookworm AS build
WORKDIR /src
RUN rustup target add x86_64-unknown-linux-musl
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `[[test]]` entries in Cargo.toml must resolve for the manifest to parse.
COPY tests ./tests
RUN cargo build --release --locked --target x86_64-unknown-linux-musl --bin pgsaci

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/pgsaci /pgsaci
# No shell / package manager / libc — nothing to run as a named user, so use a
# numeric non-root UID (valid without /etc/passwd).
USER 10001:10001
# Oracle TNS listener; health server (when PGSACI_HEALTH_ADDR is set).
EXPOSE 1521 9500
ENTRYPOINT ["/pgsaci"]
