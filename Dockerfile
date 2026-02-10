FROM --platform=$BUILDPLATFORM node:20-bookworm-slim AS web-build

WORKDIR /app
ARG PNPM_VERSION=10.10.0

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY tsconfig.json tsconfig.node.json vite.config.ts vite.web.config.ts ./
COPY tailwind.config.cjs postcss.config.cjs components.json ./
COPY src ./src
COPY web ./web

RUN npm i -g "pnpm@${PNPM_VERSION}" \
  && pnpm install --frozen-lockfile \
  && pnpm -s vite build -c vite.web.config.ts

FROM --platform=$BUILDPLATFORM rust:1.85-bookworm AS rust-build

WORKDIR /app

# Make cargo downloads more resilient in CI/BuildKit environments.
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
ENV CARGO_NET_RETRY=10
ENV CARGO_HTTP_TIMEOUT=600

# Workspace + crates (the core crate reuses `src-tauri/src/**` via #[path])
COPY Cargo.toml ./
COPY Cargo.lock ./
COPY crates ./crates
COPY src-tauri ./src-tauri

RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/usr/local/cargo/git \
  --mount=type=cache,target=/app/target \
  cargo build -p cc-switch-service --release \
  && cp /app/target/release/cc-switch-service /app/cc-switch-service

FROM debian:bookworm-slim

WORKDIR /app
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

COPY --from=rust-build /app/cc-switch-service /app/cc-switch-service
COPY --from=web-build /app/dist-web /app/dist-web

EXPOSE 8787 5000

# Defaults (override at runtime as needed)
ENV CC_SWITCH_WEB_LISTEN=0.0.0.0:8787
ENV CC_SWITCH_WEB_STATIC_DIR=/app/dist-web
ENV CC_SWITCH_WEB_ALLOW_ORIGIN=*
ENV CC_SWITCH_AUTO_START_PROXY=1
ENV CC_SWITCH_PROXY_LISTEN_PORT=5000

# Headless data lives under "home"; mount a real home dir here in Docker runtime.
ENV CC_SWITCH_TEST_HOME=/host-home
ENV HOME=/host-home

VOLUME ["/host-home"]

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/health >/dev/null || exit 1

ENTRYPOINT ["/app/cc-switch-service"]
