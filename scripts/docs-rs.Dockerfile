FROM rustlang/rust:nightly-bookworm

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        capnproto \
        clang \
        cmake \
        libcapnp-dev \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-docs-rs --locked

RUN useradd --create-home --uid 1000 docs \
    && chown --recursive docs:docs /usr/local/cargo

WORKDIR /workspace
COPY --chown=docs:docs . .

USER docs

# Populate Cargo's cache while image builds still have network access. The
# resulting image runs the documentation build with Docker networking disabled.
RUN cargo fetch --manifest-path=stratum-apps/Cargo.toml \
    && cargo fetch --manifest-path=pool-apps/pool/Cargo.toml \
    && cargo fetch --manifest-path=pool-apps/jd-server/Cargo.toml \
    && cargo fetch --manifest-path=miner-apps/jd-client/Cargo.toml \
    && cargo fetch --manifest-path=miner-apps/translator/Cargo.toml \
    && cargo fetch --manifest-path=integration-tests/Cargo.toml \
    && cargo fetch --manifest-path=bitcoin-core-sv2/Cargo.toml

ENTRYPOINT ["./scripts/docs-rs-check.sh"]
