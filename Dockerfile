# Build: docker build -t levyks/pgsaci:0.0.1 .
# Run:   docker run --rm -p 1521:1521 -e PGSACI_PG_HOST=... levyks/pgsaci:0.0.1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `[[test]]` entries in Cargo.toml must resolve for the manifest to parse, even
# for a `--bin` build; the test sources are not compiled.
COPY tests ./tests
RUN cargo build --release --locked --bin pgsaci

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 pgsaci
COPY --from=build /src/target/release/pgsaci /usr/local/bin/pgsaci
USER pgsaci
# Oracle TNS listener; health server (when PGSACI_HEALTH_ADDR is set).
EXPOSE 1521 9500
ENTRYPOINT ["pgsaci"]
