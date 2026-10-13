###
# ---- Builder Stage ----
###
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    capnproto libcapnp-dev curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# to define what app will be built
ARG APP

### caching dependencies

# `cargo fetch` resolves the whole workspace, so every member manifest has to be
# in place before the dependency layer can be built.
COPY Cargo.toml Cargo.lock ./

COPY bitcoin-core-sv2/Cargo.toml bitcoin-core-sv2/Cargo.toml
COPY bitcoin-core-sv2/src/lib.rs bitcoin-core-sv2/src/lib.rs

COPY stratum-apps/Cargo.toml stratum-apps/Cargo.toml
COPY stratum-apps/src/lib.rs stratum-apps/src/lib.rs

COPY pool-apps/pool/Cargo.toml pool-apps/pool/Cargo.toml

COPY pool-apps/jd-server/Cargo.toml pool-apps/jd-server/Cargo.toml

COPY miner-apps/jd-client/Cargo.toml miner-apps/jd-client/Cargo.toml
COPY miner-apps/translator/Cargo.toml miner-apps/translator/Cargo.toml

COPY integration-tests/Cargo.toml integration-tests/Cargo.toml

RUN echo "Fetching deps for ${APP}" && cargo fetch --locked


### copying source code

COPY pool-apps/ pool-apps/
COPY miner-apps/ miner-apps/
COPY bitcoin-core-sv2/ bitcoin-core-sv2/
COPY stratum-apps/ stratum-apps/

# build target app
RUN echo "Building ${APP}" && \
    case "$APP" in \
        pool_sv2|jd_client_sv2|translator_sv2) cargo build --release --locked -p "$APP" --target-dir ./ ;; \
        *) echo "Unknown APP: $APP" && exit 1 ;; \
    esac

###
# ---- Runtime Stage ----
###
FROM debian:bookworm-slim AS runtime

# CA certificates are needed for reqwest-based miner telemetry requests.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
ARG APP
ENV APP=${APP}

COPY --from=builder /app/release/${APP} /app/${APP}

# There is no config file: each app is configured entirely from the environment
# (<PREFIX>__<FIELD> variables supplied via docker_env). The optional config file
# the binary would otherwise read from its working directory is simply absent.
#
# `exec` replaces the shell so the app runs as PID 1 and receives the container's
# stop signal directly, allowing graceful shutdown.
ENTRYPOINT ["/bin/sh", "-c", "exec /app/${APP}"]
