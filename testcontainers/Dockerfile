# PostgreSQL + orafce, used by `tests/corpus.rs` and `clients/run.sh`.
# `PG_VERSION` selects the major (CI builds 18, 16 and 13 to prove dbSaci works
# across the supported PostgreSQL range).
#
# orafce is built from `master` (its lexer needs flex/bison). That is a moving
# target — pin `--branch` to a tag or commit if CI reproducibility matters more
# than tracking upstream.
ARG PG_VERSION=18
FROM postgres:${PG_VERSION}-bookworm
ARG PG_VERSION=18

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        postgresql-server-dev-${PG_VERSION} \
        build-essential \
        flex \
        bison \
        git \
        ca-certificates && \
    git clone --depth 1 --branch master https://github.com/orafce/orafce.git /tmp/orafce && \
    cd /tmp/orafce && \
    make USE_PGXS=1 && \
    make USE_PGXS=1 install && \
    apt-get remove -y build-essential flex bison git postgresql-server-dev-${PG_VERSION} && \
    apt-get autoremove -y && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* /tmp/orafce

CMD ["postgres"]
