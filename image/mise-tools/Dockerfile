# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# check=skip=InvalidDefaultArgInFrom
# A runner image with a repository's pinned mise and the tools of its mise.lock, on one of runnerd's runner images
# (docs/adr/0148). build.sh is the way in: it passes the arguments below, the repository's mise.toml and mise.lock as
# the build context `project`, and proves the result with no network. Everything added is root's, outside the slot's
# layers, so job code, which runs as `runner` without sudo, cannot change it, and no job sees what another changed.
ARG BASE
FROM ${BASE} AS base

# The repository's config, refused before anything is downloaded when the build cannot verify it (inputs.py check).
# The SHA-256s build.sh passes are the image's labels; checking them here keeps the labels true to the files.
FROM base AS inputs
USER root
ARG LOCK_SHA256
ARG TOML_SHA256
COPY --chmod=755 inputs.py /opt/mise/libexec/runnerd-mise-inputs
COPY --from=project --chmod=644 mise.toml mise.lock /opt/mise/share/
RUN cd /opt/mise/share \
    && printf '%s  mise.lock\n%s  mise.toml\n' "${LOCK_SHA256}" "${TOML_SHA256}" | sha256sum --check --strict \
    && /opt/mise/libexec/runnerd-mise-inputs check /opt/mise/share \
    && mkdir /opt/mise/build \
    && /opt/mise/libexec/runnerd-mise-inputs build-config mise.toml > /opt/mise/build/mise.toml \
    && cp mise.lock /opt/mise/build/mise.lock

# mise itself, by exact version from its GitHub release (mise.jdx.dev serves only the newest), checked against the
# SHA-256 of the extracted binary, which is what mise-action's `sha256` input pins, before anything runs it.
FROM base AS mise
USER root
ARG TARGETARCH
ARG MISE_VERSION
ARG MISE_SHA256
RUN [ "${TARGETARCH:-$(dpkg --print-architecture)}" = amd64 ] || { echo "linux-x64 only" >&2; exit 1; } \
    && [ -n "${MISE_VERSION}" ] && [ -n "${MISE_SHA256}" ] \
    && curl -sSfL --retry 3 -o /tmp/mise.tar.zst \
        "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-x64.tar.zst" \
    && tar --zstd -xf /tmp/mise.tar.zst -C /tmp mise/bin/mise \
    && echo "${MISE_SHA256}  /tmp/mise/bin/mise" | sha256sum --check --strict \
    && install -D -o root -g root -m 755 /tmp/mise/bin/mise /opt/mise/bin/mise \
    && rm -rf /tmp/mise /tmp/mise.tar.zst \
    && /opt/mise/bin/mise --version

# The tools, as root into mise's system installs directory, /usr/local/share/mise/installs, at the paths jobs run them
# from. --locked takes each version and URL from mise.lock, and each backend inputs.py lets through checks the download
# against the lock's checksum. mise's own data, cache, state and config are scratch, gone with the stage.
FROM inputs AS tools
COPY --from=mise /opt/mise/bin/mise /opt/mise/bin/mise
WORKDIR /opt/mise/build
RUN export MISE_DATA_DIR=/tmp/mise/data MISE_CACHE_DIR=/tmp/mise/cache MISE_STATE_DIR=/tmp/mise/state \
        MISE_CONFIG_DIR=/tmp/mise/config MISE_TRUSTED_CONFIG_PATHS=/opt/mise/build MISE_YES=1 \
    && /opt/mise/bin/mise install --system --locked \
    && rm -rf /tmp/mise \
    && chown -R root:root /usr/local/share/mise \
    && chmod -R go-w /usr/local/share/mise

FROM base
COPY --from=tools /usr/local/share/mise /usr/local/share/mise
COPY --from=tools /opt/mise/bin /opt/mise/bin
COPY --from=tools /opt/mise/libexec /opt/mise/libexec
COPY --from=tools /opt/mise/share /opt/mise/share
ARG BASE
ARG MISE_VERSION
ARG MISE_SHA256
ARG LOCK_SHA256
ARG TOML_SHA256
# The base's labels stay, runnerd.version among them: the runner's scripts are the base's (docs/adr/0148).
LABEL runnerd.mise.base="${BASE}" \
      runnerd.mise.version="${MISE_VERSION}" \
      runnerd.mise.sha256="${MISE_SHA256}" \
      runnerd.mise.lock-sha256="${LOCK_SHA256}" \
      runnerd.mise.toml-sha256="${TOML_SHA256}"
