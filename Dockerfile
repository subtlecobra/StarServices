# One image per service, all from a single build stage:
#   podman build --build-arg SERVICE=sc-login -t scbackend/sc-login:dev .
# The binaries are static musl, and Alpine is a minimal musl userland that keeps a shell for debugging.
FROM docker.io/library/rust:1-alpine AS build
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY . .
# The target directory is a cache mount, so a small change only rebuilds what it touches. Mounts are not
# part of the image, which is why the binaries are copied out.
ARG PACKAGES=--workspace
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cache/target \
    CARGO_TARGET_DIR=/cache/target cargo build --release --locked $PACKAGES \
    && mkdir -p /out \
    && find /cache/target/x86_64-unknown-linux-musl/release -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

FROM docker.io/library/alpine:3
ARG SERVICE
COPY --from=build /out/${SERVICE} /usr/local/bin/service
USER 65532:65532
ENV SCBACKEND_DATA=/data
ENTRYPOINT ["/usr/local/bin/service"]
