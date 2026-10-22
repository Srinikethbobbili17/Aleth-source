# Production image for the RSS backend. Works with Docker (BuildKit) and
# Podman, on any platform both base images publish: linux/amd64, linux/arm64,
# and others. The development image is Containerfile.dev.

FROM docker.io/library/rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
COPY src ./src
# Optional: `--build-arg CARGO_BUILD_JOBS=2` lowers peak memory on small devices.
ARG CARGO_BUILD_JOBS=default
# Cache mounts keep downloaded crates and compiled dependencies between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin rss-backend \
    && cp target/release/rss-backend /rss-backend

# Distroless: glibc and the binary only; no shell, package manager, or root user.
# HTTPS trust roots are compiled into the binary (webpki-roots).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /rss-backend /usr/local/bin/rss-backend
ENV BIND_ADDR=0.0.0.0:3000 \
    RUST_LOG=rss_backend=info,tower_http=info
EXPOSE 3000
USER nonroot
HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/rss-backend", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/rss-backend"]
