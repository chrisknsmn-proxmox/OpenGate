# Base images are pinned by digest; dependencies by Cargo.lock (--locked).
# To update: docker pull <image:tag>, then copy its digest here.

FROM rust:1.92-slim-trixie@sha256:bf3368a992915f128293ac76917ab6e561e4dda883273c8f5c9f6f8ea37a378e AS build
WORKDIR /app

# Build dependencies alone first so code changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin opengate \
    # Persistent state (encrypted ChatGPT credentials). A named volume mounted
    # here starts with this ownership and mode. Nothing secret is in the image.
    && mkdir -p /data/chatgpt \
    && chown -R 10001:10001 /data \
    && chmod 700 /data /data/chatgpt
COPY --from=build /app/target/release/opengate /usr/local/bin/opengate
USER 10001:10001
# TLS roots are compiled in (rustls + webpki-roots), so no CA package or Codex
# runtime is needed: the ChatGPT fallback is plain HTTPS from this binary.
EXPOSE 8000
# The binary checks its own /health/live, so the image needs no curl.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 CMD ["opengate", "healthcheck"]
CMD ["opengate"]
