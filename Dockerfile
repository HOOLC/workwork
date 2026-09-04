# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS rust-build
WORKDIR /src
# kache: content-addressed compile cache, persisted via the kache-store cache
# mount and shared across machines through the R2 remote when secrets are set.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      arm64) KARCH=aarch64 ;; \
      amd64) KARCH=x86_64 ;; \
      *) KARCH=aarch64 ;; \
    esac \
    && curl -fsSL --retry 2 \
      "https://github.com/kunobi-ninja/kache/releases/download/v0.16.0/kache-${KARCH}-unknown-linux-musl.tar.gz" \
      -o /tmp/kache.tar.gz \
    && tar -xzf /tmp/kache.tar.gz -C /usr/local/bin kache \
    && chmod +x /usr/local/bin/kache
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN --mount=type=secret,id=kache_s3_access_key \
    --mount=type=secret,id=kache_s3_secret_key \
    --mount=type=cache,target=/root/.cache/kache \
    bash -c ' \
      set -euo pipefail; \
      if [ -s /run/secrets/kache_s3_access_key ] && [ -s /run/secrets/kache_s3_secret_key ]; then \
        export KACHE_S3_ACCESS_KEY="$(cat /run/secrets/kache_s3_access_key)"; \
        export KACHE_S3_SECRET_KEY="$(cat /run/secrets/kache_s3_secret_key)"; \
        export RUSTC_WRAPPER=kache; \
        kache sync --pull --allow-partial || echo "kache pull failed, cold build" >&2; \
      fi; \
      cargo build --release -p zork -p zork-gateway -p zork-agent -p zork-call; \
      if [ -n "${RUSTC_WRAPPER:-}" ]; then kache sync --push --allow-partial || echo "kache push failed" >&2; fi \
    '

FROM node:22-bookworm AS node-build
RUN npm install -g vite-plus@0.1.20
WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/admin-ui/package.json ./apps/admin-ui/package.json
COPY packages/zork/package.json ./packages/zork/package.json
RUN vp install --frozen-lockfile --filter @zork/admin-ui
COPY tsconfig.json vite.config.ts ./
COPY apps ./apps
COPY packages ./packages
COPY scripts/build ./scripts/build
RUN vp run --filter @zork/admin-ui build

FROM node:22-bookworm-slim AS node
WORKDIR /app
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl git gh python3 ripgrep \
  && rm -rf /var/lib/apt/lists/*
COPY --from=rust-build /src/target/release/zork /usr/local/bin/zork
COPY --from=rust-build /src/target/release/zork-gateway /usr/local/bin/zork-gateway
COPY --from=rust-build /src/target/release/zork-agent /usr/local/bin/zork-agent
COPY --from=rust-build /src/target/release/zork-call /usr/local/bin/zork-call
COPY --from=rust-build /src/target/release/zork-gh /usr/local/bin/zork-gh
COPY --from=node-build /app/apps/admin-ui/dist /ui
EXPOSE 18790 3000 3001
CMD ["zork", "start", "--data", "/data", "--listen", "0.0.0.0"]
