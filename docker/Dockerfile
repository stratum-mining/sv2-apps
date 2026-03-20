###
# ---- Builder Stage ----
###
FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    capnproto libcapnp-dev curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# to define what app will be built
ARG APP

### caching dependencies

COPY bitcoin-core-sv2/Cargo.toml bitcoin-core-sv2/Cargo.toml
COPY bitcoin-core-sv2/src/lib.rs bitcoin-core-sv2/src/lib.rs

COPY stratum-apps/Cargo.toml stratum-apps/Cargo.toml
COPY stratum-apps/src/lib.rs stratum-apps/src/lib.rs

COPY pool-apps/pool/Cargo.toml pool-apps/pool/Cargo.toml

COPY pool-apps/jd-server/Cargo.toml pool-apps/jd-server/Cargo.toml

COPY miner-apps/jd-client/Cargo.toml miner-apps/jd-client/Cargo.toml
COPY miner-apps/translator/Cargo.toml miner-apps/translator/Cargo.toml

RUN echo "Fetching deps for ${APP}" && \
    case "$APP" in \
        pool_sv2) cargo fetch --manifest-path=pool-apps/pool/Cargo.toml ;; \
        jd_client_sv2) cargo fetch --manifest-path=miner-apps/jd-client/Cargo.toml ;; \
        translator_sv2) cargo fetch --manifest-path=miner-apps/translator/Cargo.toml ;; \
        *) echo "Unknown APP: $APP" && exit 1 ;; \
    esac


### copying source code

COPY pool-apps/ pool-apps/
COPY miner-apps/ miner-apps/
COPY bitcoin-core-sv2/ bitcoin-core-sv2/
COPY stratum-apps/ stratum-apps/

# build target app
RUN echo "Building ${APP}" && \
    case "$APP" in \
        pool_sv2) cargo build --release --manifest-path pool-apps/pool/Cargo.toml --target-dir ./ ;; \
        jd_client_sv2) cargo build --release --manifest-path miner-apps/jd-client/Cargo.toml --target-dir ./ ;; \
        translator_sv2) cargo build --release --manifest-path miner-apps/translator/Cargo.toml --target-dir ./ ;; \
        *) echo "Unknown APP: $APP" && exit 1 ;; \
    esac

###
# ---- Runtime Stage ----
###
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    gettext-base && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
ARG APP
ENV APP=${APP}

COPY --from=builder /app/release/${APP} /app/${APP}

ENTRYPOINT ["/bin/sh", "-c", "/app/${APP}"]
