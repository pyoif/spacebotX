# ---- Builder stage ----
# Compiles the React frontend and the Rust binary with the frontend embedded.
FROM rust:bookworm AS builder

# Install build dependencies:
#   protobuf-compiler — LanceDB protobuf codegen
#   cmake — onig_sys (regex), lz4-sys
#   libssl-dev — openssl-sys (reqwest TLS)
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    libprotobuf-dev \
    cmake \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://bun.sh/install | bash
ENV PATH="/root/.bun/bin:${PATH}"

# Node 22+ is required for the OpenCode embed Vite build.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 1. Fetch and cache Rust dependencies.
#    cargo fetch needs a valid target, so we create stubs that get replaced later.
COPY Cargo.toml Cargo.lock ./
COPY vendor/ vendor/
RUN mkdir -p src/bin && echo "fn main() {}" > src/main.rs && touch src/lib.rs \
    && echo "fn main() {}" > src/bin/openapi_spec.rs \
    && cargo build --release --features metrics \
    && rm -rf src

# 2. Install frontend dependencies.
COPY interface/package.json interface/
RUN cd interface && bun install

# 3. Build the OpenCode embed bundle (live coding UI in Workers tab).
#    Must run before the frontend build so the embed assets in
#    interface/public/opencode-embed/ are included in the Vite output.
COPY scripts/build-opencode-embed.sh scripts/
COPY interface/opencode-embed-src/ interface/opencode-embed-src/
RUN ./scripts/build-opencode-embed.sh

# 4. Build the frontend (includes OpenCode embed assets from step 3).
COPY interface/ interface/
RUN cd interface && bun run build

# 5. Copy source and compile the real binary.
#    build.rs is skipped (SPACEBOT_SKIP_FRONTEND_BUILD=1) since the
#    frontend is already built above with the OpenCode embed included.
#    prompts/ is needed for include_str! in src/prompts/text.rs.
#    presets/ is needed for rust-embed in src/factory/presets.rs and
#    include_str! in src/identity/files.rs.
#    migrations/ is needed for sqlx::migrate! in src/db.rs.
#    docs/ is needed for rust-embed in src/self_awareness.rs.
#    AGENTS.md, README.md, CHANGELOG.md are needed for include_str! in src/self_awareness.rs.
COPY build.rs ./
COPY prompts/ prompts/
COPY presets/ presets/
COPY skills/ skills/
COPY migrations/ migrations/
COPY docs/ docs/
COPY AGENTS.md README.md CHANGELOG.md ./
COPY src/ src/
RUN SPACEBOT_SKIP_FRONTEND_BUILD=1 cargo build --release --features metrics \
    && mv /build/target/release/spacebot /usr/local/bin/spacebot \
    && cargo clean -p spacebot --release --target-dir /build/target

# ---- Runtime stage ----
# Minimal runtime with Chrome runtime libraries for fetcher-downloaded Chromium.
# Chrome itself is downloaded on first browser tool use and cached on the volume.
#
# Base is Chainguard Wolfi (glibc-based, apk packages) instead of Debian. The
# package list below mirrors the previous apt list; every name was verified
# against https://packages.wolfi.dev/os/x86_64/APKINDEX.tar.gz:
#   apt libsqlite3-0 -> sqlite-libs (provides so:libsqlite3.so.0)
#   apt libatk-bridge2.0-0 -> libatk-bridge-2.0, libgbm1 -> mesa-gbm,
#   libdrm2 -> libdrm-libs, libasound2 -> alsa-lib, libpango-1.0-0 -> pango,
#   libcairo2 -> cairo, libcups2 -> cups-libs, libnss3 -> libnss,
#   fonts-liberation -> font-liberation, gh -> gh.
#   libudev.so.1 has no apt equivalent in the original list, but Chromium
#   (Chrome for Testing 153, build 1243) links against it and fails to start
#   without it; Wolfi ships it as the `libudev` package (provides so:libudev.so.1).
# Also installs the Node toolchain (nodejs-22 + npm) so that `nub`/`npm`/`npx`
# are available for stdio MCP servers, plus git (shelled out to by `nubx` when
# resolving `github:` specs) and libatomic (needed by nub's bundled Node
# runtime). Bash is present for scripts with a bash shebang.
FROM cgr.dev/chainguard/wolfi-base:latest

# Chrome runtime dependencies below are required whether Chrome is
# system-installed or downloaded by the built-in fetcher. The fetcher provides
# the browser binary; these are the shared libraries it links against.
# (Comment kept OUT of the multi-line apk command: a `#` inside a
# backslash-continued RUN list truncates the command and breaks the build.)
RUN apk add --no-cache \
    ca-certificates \
    ca-certificates-bundle \
    sqlite-libs \
    curl \
    gh \
    bubblewrap \
    openssh-server \
    openssh-client \
    bash \
    git \
    libatomic \
    nodejs-22 \
    npm \
    font-liberation \
    libnss \
    libudev \
    libatk-bridge-2.0 \
    libdrm-libs \
    libxcomposite \
    libxdamage \
    libxrandr \
    mesa-gbm \
    alsa-lib \
    pango \
    cairo \
    cups-libs \
    libxkbcommon \
    libxss \
    libxtst \
    libxfixes \
    && npm install -g nub \
    && npm cache clean --force \
    && rm -rf /root/.npm /tmp/*

COPY --from=builder /usr/local/bin/spacebot /usr/local/bin/spacebot
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV SPACEBOT_DIR=/data
ENV SPACEBOT_DEPLOYMENT=docker
EXPOSE 19898 18789 9090

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -f http://localhost:19898/api/health || exit 1

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["spacebot", "start", "--foreground"]
