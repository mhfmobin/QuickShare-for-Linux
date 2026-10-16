# Isolated build/test env: nothing gets installed on the host.
FROM ubuntu:26.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl build-essential pkg-config \
      libgtk-4-dev libadwaita-1-dev libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt \
    && mkdir -p /opt/cargo/registry && chmod -R a+rwX /opt/cargo /opt/rustup
WORKDIR /src
# headless screenshots of the UI (virtual X display)
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xvfb x11-apps xdotool fonts-cantarell adwaita-icon-theme \
    && rm -rf /var/lib/apt/lists/*
