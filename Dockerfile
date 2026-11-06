FROM rust:1.99.1-bookworm AS dev
WORKDIR /app
ENV KIKIMIMI_BIND=0.0.0.0 \
    KIKIMIMI_PORT=8765 \
    KIKIMIMI_DATA_DIR=/data \
    KIKIMIMI_KNOWLEDGE_ROOT=/knowledge \
    KIKIMIMI_WEB_DIR=/app/web/dist \
    KIKIMIMI_NO_BROWSER=1
CMD ["cargo", "run"]

# 画面（React + Vite）。依存は package-lock.json の版に固定して入れる。
FROM node:22-alpine AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm,sharing=locked npm ci --no-audit --no-fund
COPY web ./
RUN npm run build

FROM rust:1.99.1-bookworm AS build
WORKDIR /app
# この VPS はメモリ 2GB のため、並列コンパイルで落ちないよう 1 本に制限する。
ENV CARGO_BUILD_JOBS=1
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    touch src/lib.rs src/main.rs \
    && cargo build --release --locked \
    && cp target/release/kikimimi /usr/local/bin/kikimimi

FROM debian:bookworm-slim AS runtime
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends curl tzdata
RUN groupadd --system --gid 65532 app \
    && useradd --system --uid 65532 --gid app --home-dir /data --shell /usr/sbin/nologin app \
    && mkdir -p /data /knowledge \
    && chown app:app /data /knowledge
COPY --from=build --link --chown=65532:65532 /usr/local/bin/kikimimi /usr/local/bin/kikimimi
COPY --from=web --link /web/dist /usr/local/share/kikimimi/web
USER app
ENV KIKIMIMI_BIND=0.0.0.0 \
    KIKIMIMI_PORT=8765 \
    KIKIMIMI_DATA_DIR=/data \
    KIKIMIMI_KNOWLEDGE_ROOT=/knowledge \
    KIKIMIMI_WEB_DIR=/usr/local/share/kikimimi/web \
    KIKIMIMI_NO_BROWSER=1 \
    TZ=Asia/Tokyo
EXPOSE 8765
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8765/health || exit 1
ENTRYPOINT ["/usr/local/bin/kikimimi"]
