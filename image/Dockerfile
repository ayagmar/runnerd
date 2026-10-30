# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# An ephemeral GitHub Actions runner with its own Docker Engine (Docker-in-Docker), so every container a job starts
# stays inside its runner container's CPU and memory limits and under runnerd's slice. `base` holds what any workflow
# needs and runnerd's scripts; `android` adds a JDK and the Android SDK; `jvm` and `node` build on `android`. A
# profile's image or image_stage in runnerd.toml picks its stage (docs/adr/0035).
FROM docker:29.9.2-dind@sha256:9e79b879b177fbd72c34376a3dd3d91747da5db49e1f560f76aea59561471977 AS docker

FROM ghcr.io/actions/actions-runner:2.338.1@sha256:e275409999c37162649aa050ca573f03eab2eabcb3a1e7fcc47548de6b4f9690 AS base

USER root

# Every apt-get in this image and the stages on top resolves against Ubuntu's archive as it was at this moment
# (snapshot.ubuntu.com), so a rebuild installs the same package versions until someone moves the date. apt also maps the base image's git-core
# PPA to its snapshot (snapshot.ppa.launchpadcontent.net). docs/adr/0023 says how to bump.
ARG UBUNTU_SNAPSHOT=20261022T000000Z
RUN echo "APT::Snapshot \"${UBUNTU_SNAPSHOT}\";" > /etc/apt/apt.conf.d/50runnerd-snapshot

# What the workflows expect from GitHub's ubuntu-24.04 image and the base image lacks: iptables for dockerd, make,
# xz-utils and zstd (setup-* archives, actions/cache) and zip; and socat, which forwards the build cache's socket
# (build-cache.sh), with iptables keeping GitHub Actions' cache's port from other containers.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        iptables make socat xz-utils zip zstd \
    && rm -rf /var/lib/apt/lists/*

# Docker Engine, CLI, Compose and Buildx from the official image, replacing the older CLI of the base image.
COPY --from=docker \
    /usr/local/bin/dind /usr/local/bin/dockerd /usr/local/bin/docker /usr/local/bin/docker-init \
    /usr/local/bin/docker-proxy /usr/local/bin/containerd-shim-runc-v2 /usr/local/bin/ctr /usr/local/bin/runc \
    /usr/local/bin/
COPY --from=docker /usr/local/libexec/docker/cli-plugins/ /usr/local/lib/docker/cli-plugins/
RUN rm /usr/bin/docker

# GitHub CLI, for workflows that comment on pull requests or file issues. The SHA-256 is from the release's
# published checksums file.
ARG GH_VERSION=2.102.0
ARG GH_SHA256=bb766f710eef8ede859c18578c72c327597cd4c8a85b06001b1f3843c6019386
RUN curl -sSfL -o /tmp/gh.tar.gz \
        "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" \
    && echo "${GH_SHA256}  /tmp/gh.tar.gz" | sha256sum --check \
    && tar -xzf /tmp/gh.tar.gz -C /usr/local --strip-components=1 "gh_${GH_VERSION}_linux_amd64/bin/gh" \
    && rm /tmp/gh.tar.gz

# Job code runs as `runner` without sudo, so it cannot read the root entrypoint's environment or reconfigure its
# container. Its one root command is the cleanup after each job (job-cleanup.sh), which takes no input from the job.
# The base image's sudoers reads no drop-in directory, hence the rule in the file itself.
RUN gpasswd -d runner sudo \
    && echo 'runner ALL=(root) NOPASSWD: /usr/local/sbin/runnerd-job-cleanup' >> /etc/sudoers \
    && visudo -c

# Package caches. Everything under ~/.cache is the slot's layer over the profile's snapshot (entrypoint.sh).
# The pnpm store and the Maven repository are volumes all slots share, because both tools support concurrent
# use of one store on one machine: pnpm when its processes share the lock directory under XDG_RUNTIME_DIR, Maven
# with the resolver's file locks (file-lock needs the file-gav name mapper). Podman, Buildah and Skopeo keep registry
# logins under XDG_RUNTIME_DIR, which every slot of every profile shares: REGISTRY_AUTH_FILE keeps them on the
# container's own disk, which goes with it.
ENV GRADLE_USER_HOME=/home/runner/.cache/gradle \
    MAVEN_USER_HOME=/home/runner/.cache/m2 \
    MAVEN_OPTS="-Dmaven.repo.local=/shared/maven -Daether.syncContext.named.factory=file-lock -Daether.syncContext.named.nameMapper=file-gav" \
    pnpm_config_store_dir=/shared/pnpm/store \
    XDG_RUNTIME_DIR=/shared/pnpm/run \
    REGISTRY_AUTH_FILE=/home/runner/.config/containers/auth.json \
    npm_config_cache=/home/runner/.cache/npm \
    PUB_CACHE=/home/runner/.cache/pub \
    PIPX_HOME=/home/runner/.cache/pipx
# The toolchain homes whose default is outside ~/.cache: rustup's toolchains and cargo's crates and installs, Go's
# modules (GOMODCACHE is $GOPATH/pkg/mod), Bun's packages and Yarn 2+'s. pip, uv, Poetry, Yarn 1, Deno, coursier
# and Go's build cache already default to ~/.cache. A job's credentials and settings in these homes outlive it in
# the slot's layer; layers.sh lists the files that go after each job. A warm rustup is on the PATH, so
# rust-toolchain actions find it instead of installing another.
ENV CARGO_HOME=/home/runner/.cache/cargo \
    RUSTUP_HOME=/home/runner/.cache/rustup \
    GOPATH=/home/runner/.cache/go \
    BUN_INSTALL_CACHE_DIR=/home/runner/.cache/bun \
    YARN_GLOBAL_FOLDER=/home/runner/.cache/yarn/berry \
    PATH=/home/runner/.cache/cargo/bin:$PATH

# Mount points. A new volume takes the owner of its mount point. The base image leaves /home/runner writable by
# everyone; SWC refuses a cache root under such a parent.
RUN chmod 755 /home/runner \
    && mkdir -p /home/runner/.cache /home/runner/_work/_tool /shared/pnpm /shared/maven /base /state \
    && chown -R runner:runner /home/runner/.cache /home/runner/_work /shared

COPY daemon.json /etc/docker/daemon.json
COPY layers.sh /usr/local/lib/runnerd-layers.sh
COPY build-cache.sh /usr/local/lib/runnerd-build-cache.sh
COPY build-cache.gradle /usr/local/share/runnerd/build-cache.gradle
COPY actions-cache.cjs /usr/local/lib/runnerd-actions-cache.cjs
COPY job-cleanup.sh /usr/local/sbin/runnerd-job-cleanup
COPY job-started.sh /usr/local/bin/runnerd-job-started.sh
COPY job-completed.sh /usr/local/bin/runnerd-job-completed.sh
COPY seed.sh /usr/local/sbin/runnerd-seed
COPY scrub.sh /usr/local/sbin/runnerd-scrub
COPY entrypoint.sh /usr/local/bin/runnerd-runner
# The runner accepts hook scripts only by their extension.
ENV ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/bin/runnerd-job-started.sh \
    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/bin/runnerd-job-completed.sh

ENTRYPOINT ["dind", "runnerd-runner"]

FROM base AS android

# A JDK 17 for sdkmanager and for Android builds whose workflow sets up no JDK (`flutter build appbundle`), as on
# GitHub's ubuntu-24.04 image. A workflow's setup-java takes precedence through JAVA_HOME and PATH.
RUN apt-get update \
    && apt-get install -y --no-install-recommends openjdk-17-jdk-headless \
    && rm -rf /var/lib/apt/lists/*

# Android SDK command-line tools and platform-tools, plus the accepted SDK license as the file sdkmanager writes when
# it installs a package. Android builds download the platforms, build tools and NDK they need into the slot's
# layer of the SDK (entrypoint.sh). The SHA-256s were computed from Google's archives, whose SHA-1s match Google's
# repository index (repository2-3.xml).
ARG ANDROID_CMDLINE_TOOLS=16111833
ARG ANDROID_CMDLINE_TOOLS_SHA256=0877a1d048fe4a24efe2eff536ca4223f7adeb58648bb81909d33c446918cfa8
ARG ANDROID_PLATFORM_TOOLS=37.0.1
ARG ANDROID_PLATFORM_TOOLS_SHA256=d230f13842f60f782a8645f9c813f8f845bf36089ea7289f28c48f17979313f1
ARG ANDROID_SDK_LICENSE=24333f8a63b6825ea9c5514f83c2829b004d1fee
ENV ANDROID_HOME=/opt/android-sdk ANDROID_SDK_ROOT=/opt/android-sdk
RUN curl -sSfL -o /tmp/cmdline-tools.zip \
        "https://dl.google.com/android/repository/commandlinetools-linux-${ANDROID_CMDLINE_TOOLS}_latest.zip" \
    && echo "${ANDROID_CMDLINE_TOOLS_SHA256}  /tmp/cmdline-tools.zip" | sha256sum --check \
    && curl -sSfL -o /tmp/platform-tools.zip \
        "https://dl.google.com/android/repository/platform-tools_r${ANDROID_PLATFORM_TOOLS}-linux.zip" \
    && echo "${ANDROID_PLATFORM_TOOLS_SHA256}  /tmp/platform-tools.zip" | sha256sum --check \
    && mkdir -p "$ANDROID_HOME/cmdline-tools" "$ANDROID_HOME/licenses" \
    && unzip -q /tmp/cmdline-tools.zip -d "$ANDROID_HOME/cmdline-tools" \
    && mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest" \
    && unzip -q /tmp/platform-tools.zip -d "$ANDROID_HOME" \
    && printf '\n%s' "$ANDROID_SDK_LICENSE" > "$ANDROID_HOME/licenses/android-sdk-license" \
    && rm /tmp/cmdline-tools.zip /tmp/platform-tools.zip \
    && chown -R runner:runner "$ANDROID_HOME"

FROM android AS jvm

FROM android AS node

# pipx (diff-cover) and the system libraries of Playwright's Chromium headless shell on Ubuntu 24.04 (`playwright
# install-deps --dry-run chromium-headless-shell`, Playwright 1.64), so ci.yml installs only the browser on these
# runners. When a Playwright update needs another library, Chromium fails to start and names it: add it here.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pipx \
        libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64 libcairo2 libcups2t64 libdrm2 \
        libgbm1 libpango-1.0-0 libxcomposite1 libxdamage1 libxfixes3 libxkbcommon0 libxrandr2 xvfb \
        fonts-noto-color-emoji fonts-unifont libfontconfig1 libfreetype6 xfonts-cyrillic xfonts-scalable \
        fonts-liberation fonts-ipafont-gothic fonts-wqy-zenhei fonts-tlwg-loma-otf fonts-freefont-ttf \
    && rm -rf /var/lib/apt/lists/*
