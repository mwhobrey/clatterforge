# Linux build environment for the desktop installer (#38): Debian with the WebKitGTK build
# dependencies Tauri needs, plus the same tauri-cli and dioxus-cli versions used on Windows.
#   docker build -f scripts/release/linux-build.Dockerfile -t clatterforge-linux-build scripts/release
FROM rust@sha256:64232e656c058f4468e8d024e990acff04f0fd5a5c0a88a574dc37773d7325c9

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
        build-essential file xdg-utils python3 git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
RUN cargo install --locked tauri-cli --version 2.12.4 \
    && cargo install --locked dioxus-cli --version 0.7.10 \
    && rm -rf /usr/local/cargo/registry
