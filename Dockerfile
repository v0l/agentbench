# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p agentbench-server --no-default-features \
    && install -Dm755 target/release/agentbench-server /out/agentbench-server

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash build-essential ca-certificates curl git openssh-client pkg-config tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --user-group --create-home --home-dir /home/agent --shell /bin/bash agent \
    && mkdir -p /data/git \
    && chown -R agent:agent /data
COPY --from=build /out/agentbench-server /usr/local/bin/agentbench-server
USER agent
ENV AGENTBENCH_HOME=/data \
    AGENTBENCH_MEMORY=/data/memory \
    AGENTBENCH_LISTEN=0.0.0.0:7420
WORKDIR /data/git
VOLUME /data
EXPOSE 7420
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7420/health || exit 1
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["agentbench-server", "--listen", "0.0.0.0:7420", "serve"]
