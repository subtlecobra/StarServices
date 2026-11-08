# One image per service, all from a single build stage (scripts/build-images.sh):
#   podman build --build-arg SERVICE=sc-login --build-arg VERSION=0.1.0 -t scbackend/sc-login:0.1.0 .
# The binaries are static musl, and Alpine is a minimal musl userland that keeps a shell for debugging. Both base images
# are pinned by digest, with their version beside: a new one is a deliberate change here, not whatever the tag points at.
# rust 1.99.1-alpine3.24
FROM docker.io/library/rust:1.99.1-alpine3.24@sha256:44819b27be4c97b0e4d7203bd2e02c4820f97700211b6a37fb4c246a2de59383 AS build
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY . .
# The target directory is a cache mount, so a small change only rebuilds what it touches. Mounts are not
# part of the image, which is why the binaries are copied out.
# The default members: every crate and service, not the tools (which read the game archive on a workstation).
ARG PACKAGES=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cache/target \
    CARGO_TARGET_DIR=/cache/target cargo build --release --locked $PACKAGES \
    && mkdir -p /out \
    && find /cache/target/x86_64-unknown-linux-musl/release -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

# alpine 3.24.4
FROM docker.io/library/alpine:3.24.4@sha256:93b41ef40b1677fce8fcc64ff4af09daddfacf5040afde618bc46c1e2d1cc7d4
ARG SERVICE
ARG VERSION
COPY --from=build /out/${SERVICE} /usr/local/bin/service
USER 65532:65532
ENV SCBACKEND_DATA=/data
LABEL org.opencontainers.image.title=${SERVICE} org.opencontainers.image.version=${VERSION}
ENTRYPOINT ["/usr/local/bin/service"]
