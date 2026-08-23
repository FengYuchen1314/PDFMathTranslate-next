# syntax=docker/dockerfile:1.7

FROM node:24-bookworm-slim@sha256:3638d9a6fe4030bd716be989438248074489337ba3275657f93595428be4fc03 AS frontend-build
WORKDIR /src/frontend
COPY frontend/package*.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

FROM rust:1.96-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS rust-build
WORKDIR /src/backend-rs
COPY backend-rs/Cargo.toml backend-rs/Cargo.lock ./
COPY backend-rs/migrations ./migrations
COPY backend-rs/src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 app \
    && mkdir -p /app/frontend /data/jobs /data/state \
    && chown -R app:app /app /data

COPY --from=rust-build /src/backend-rs/target/release/pdf2zh-web /usr/local/bin/pdf2zh-web
COPY --from=frontend-build /src/frontend/dist/ /app/frontend/

ENV PORT=7860 \
    PDF2ZH_DATA_ROOT=/data/jobs \
    PDF2ZH_STATE_ROOT=/data/state \
    PDF2ZH_FRONTEND_DIR=/app/frontend \
    RUST_LOG=pdf2zh_web=info,tower_http=info

USER app
EXPOSE 7860
CMD ["pdf2zh-web"]
