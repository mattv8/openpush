FROM rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730

ARG DEV_UID=1000
ARG DEV_GID=1000
ARG NODE_VERSION=24.21.0
RUN test "${DEV_UID}" != 0 \
    && apt-get update && apt-get install --yes --no-install-recommends ca-certificates curl python3 tar xz-utils util-linux build-essential pkg-config libssl-dev libsodium-dev libsqlite3-dev libpq-dev \
    && rm -rf /var/lib/apt/lists/* \
    && if ! getent group "${DEV_GID}" >/dev/null; then groupadd --gid "${DEV_GID}" developer; fi \
    && if ! getent passwd "${DEV_UID}" >/dev/null; then useradd --uid "${DEV_UID}" --gid "${DEV_GID}" --home-dir /home/developer --no-create-home --shell /bin/bash developer; fi \
    && case "$(dpkg --print-architecture)" in amd64) node_arch=x64 ;; arm64) node_arch=arm64 ;; *) echo "unsupported Node architecture" >&2; exit 1 ;; esac \
    && cd /tmp && node_archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && curl --fail --location --silent --show-error -O "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt" \
    && curl --fail --location --silent --show-error -O "https://nodejs.org/dist/v${NODE_VERSION}/${node_archive}" \
    && grep "  ${node_archive}$" SHASUMS256.txt | sha256sum -c - \
    && tar -xJf "${node_archive}" --strip-components=1 -C /usr/local \
    && rm -f SHASUMS256.txt "${node_archive}" \
    && mkdir -p /workspace/target /artifacts /home/developer/.cargo /home/developer/.local/share/pnpm /home/developer/.cache/corepack \
    && chown -R "${DEV_UID}:${DEV_GID}" /workspace /artifacts /home/developer \
    && COREPACK_HOME=/home/developer/.cache/corepack corepack enable \
    && COREPACK_HOME=/home/developer/.cache/corepack corepack prepare pnpm@12.8.1 --activate
COPY infra/dev/container-run.sh /usr/local/bin/run
RUN chmod 755 /usr/local/bin/run \
    && chown -R "${DEV_UID}:${DEV_GID}" /home/developer/.cache/corepack
USER ${DEV_UID}:${DEV_GID}
ENV HOME=/home/developer RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/developer/.cargo CARGO_TARGET_DIR=/workspace/target PNPM_HOME=/home/developer/.local/share/pnpm COREPACK_HOME=/home/developer/.cache/corepack PATH=/home/developer/.local/share/pnpm:/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN pnpm --version
WORKDIR /workspace
CMD ["sleep", "infinity"]
