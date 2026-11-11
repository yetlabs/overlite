# The image release.yml publishes: the static Linux binary that release built for
# the platform, alone. The Dockerfile at the root builds the same image from source.
FROM scratch
ARG TARGETARCH
COPY overlite-linux-${TARGETARCH} /overlite
# The databases live here: mount a host directory on it (`-v ./data:/data`).
WORKDIR /data
EXPOSE 5432
# The wildcard address, as in the root Dockerfile: 127.0.0.1 inside a container
# is reachable only from the container itself.
ENTRYPOINT ["/overlite", "--host", "0.0.0.0"]
