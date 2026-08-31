# PostgreSQL + orafce, used by `tests/corpus.rs` and `clients/run.sh`.
# `PG_VERSION` selects the major (CI builds 18, 16 and 13 to prove pgSaci works
# across the supported PostgreSQL range).
ARG PG_VERSION=18
FROM postgres:${PG_VERSION}-bookworm
ARG PG_VERSION=18

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        postgresql-server-dev-${PG_VERSION} \
        build-essential \
        git \
        ca-certificates && \
    git clone --depth 1 --branch master https://github.com/orafce/orafce.git /tmp/orafce && \
    cd /tmp/orafce && \
    make USE_PGXS=1 && \
    make USE_PGXS=1 install && \
    apt-get remove -y build-essential git postgresql-server-dev-${PG_VERSION} && \
    apt-get autoremove -y && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* /tmp/orafce

CMD ["postgres"]
