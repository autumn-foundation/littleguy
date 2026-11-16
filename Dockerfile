# littleguy in a container: `lg` and the runtimes that `run_code` uses.
#
# The `process` sandbox backend runs only on Linux, so this image is how a
# Mac runs code with littleguy. The container is the boundary of that code.
# See "Run it in a container" in the README.

FROM rust:1.99.1-bookworm AS build
# aws-lc-sys builds C code with cmake.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
# The image has the pinned toolchain. This skips the components that
# rust-toolchain.toml asks for, which a build does not need.
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --all-features --locked --bin lg \
    && cp target/release/lg /usr/local/bin/lg

FROM debian:bookworm-slim
# python3, node, bash and sh for run_code; socat for the page forwarder.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates python3 nodejs bash socat tini \
    && rm -rf /var/lib/apt/lists/*
# The config folder belongs to root, so code in the sandbox, which runs as
# lg, cannot rewrite lg.toml (ADR 0032). The daemon writes only the skills
# folder there.
RUN useradd --create-home --uid 1000 lg \
    && mkdir -p /data/config/littleguy/skills /data/share/littleguy \
    && chown -R lg:lg /data/share /data/config/littleguy/skills
COPY --from=build /usr/local/bin/lg /usr/local/bin/lg
USER lg
# The config, the databases, the socket and the workspace all live in /data.
# USER names the owner when lg.toml names none, so an empty volume starts.
ENV XDG_CONFIG_HOME=/data/config \
    XDG_DATA_HOME=/data/share \
    USER=lg
VOLUME /data
ENTRYPOINT ["tini", "--", "lg"]
CMD ["serve"]
