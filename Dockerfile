# The builder is pinned to the *build* platform and cross-compiles to the target
# instead of being emulated: compiling SQLite under QEMU takes many minutes, a
# cross compiler takes one. The binary links static, so the final image needs
# nothing around it — no libc, no shell, no package manager.
FROM --platform=$BUILDPLATFORM docker.io/library/rust:1-bookworm AS build
ARG BUILDARCH
ARG TARGETARCH
ARG VERSION=dev
RUN case "$TARGETARCH" in \
      arm64) t=aarch64-unknown-linux-gnu; p=aarch64-linux-gnu; g=aarch64-linux-gnu ;; \
      amd64) t=x86_64-unknown-linux-gnu;  p=x86_64-linux-gnu;  g=x86-64-linux-gnu ;; \
      *) echo "unsupported architecture $TARGETARCH"; exit 1 ;; \
    esac; \
    echo "$t" > /target-triple; echo "$p" > /gcc-prefix; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update -qq && apt-get install -y -qq "gcc-$g" "libc6-dev-$TARGETARCH-cross" >/dev/null; \
      # This libm.a is a linker script naming a host path the cross sysroot lacks.
      sed -i "s|/usr/lib/$p/||g" "/usr/$p/lib/libm.a"; \
    fi; \
    rustup target add "$t"
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY .cargo .cargo
COPY src src
COPY crates crates
COPY tests/fixtures tests/fixtures
RUN t=$(cat /target-triple); p=$(cat /gcc-prefix); T=$(echo "$t" | tr 'a-z-' 'A-Z_'); \
    export "CC_$(echo "$t" | tr '-' '_')=$p-gcc" "CARGO_TARGET_${T}_LINKER=$p-gcc" OVERLITE_VERSION="$VERSION" \
      RUSTFLAGS="-C target-feature=+crt-static"; \
    cargo build --release --target "$t" && cp "target/$t/release/overlite" /overlite

FROM scratch
COPY --from=build /overlite /overlite
# The databases live here: mount a host directory on it (`-v ./data:/data`), so
# a bare file name on the command line resolves into it and the -wal and -shm
# files SQLite keeps beside each database land on the host too.
WORKDIR /data
EXPOSE 5432
# 127.0.0.1 inside a container is reachable only from the container itself, so
# the image has to bind the wildcard address to be of any use. Arguments given
# to `docker run` are appended, which leaves `overlite sample.db` reading as it
# does outside a container.
ENTRYPOINT ["/overlite", "--host", "0.0.0.0"]
