FROM ubuntu@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3

ARG DEV_UID=1000
ARG DEV_GID=1000
ARG ANDROID_CMDLINE_TOOLS_VERSION=11076708
ARG RUST_VERSION=1.98.1
ENV DEBIAN_FRONTEND=noninteractive \
    ANDROID_SDK_ROOT=/opt/android-sdk \
    ANDROID_HOME=/opt/android-sdk \
    ANDROID_NDK_HOME=/opt/android-sdk/ndk/27.2.12479018 \
    HOME=/home/peppy \
    CARGO_HOME=/home/peppy/.cargo \
    RUSTUP_HOME=/home/peppy/.rustup \
    GRADLE_USER_HOME=/home/peppy/.gradle \
    CARGO_TARGET_DIR=/workspace/target

RUN test "$DEV_UID" != 0 && test "$DEV_GID" != 0 \
 && apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip rsync git build-essential pkg-config libssl-dev libsodium-dev openjdk-17-jdk-headless && rm -rf /var/lib/apt/lists/* \
 && (getent group "$DEV_GID" >/dev/null || groupadd --gid "$DEV_GID" peppy) \
 && (getent passwd "$DEV_UID" >/dev/null || useradd --uid "$DEV_UID" --gid "$DEV_GID" --home-dir "$HOME" --no-create-home --shell /bin/bash peppy) \
 && mkdir -p "$ANDROID_SDK_ROOT/cmdline-tools" "$HOME/.cargo" "$HOME/.rustup" "$HOME/.gradle" "$HOME/.android" /workspace/target /artifacts \
 && curl --fail --location --retry 3 "https://dl.google.com/android/repository/commandlinetools-linux-${ANDROID_CMDLINE_TOOLS_VERSION}_latest.zip" -o /tmp/tools.zip \
 && unzip -q /tmp/tools.zip -d "$ANDROID_SDK_ROOT/cmdline-tools" \
 && mv "$ANDROID_SDK_ROOT/cmdline-tools/cmdline-tools" "$ANDROID_SDK_ROOT/cmdline-tools/latest" \
 && rm /tmp/tools.zip \
 && chown -R "$DEV_UID:$DEV_GID" /opt/android-sdk /workspace /artifacts /home/peppy

ENV PATH=/opt/android-sdk/cmdline-tools/latest/bin:/opt/android-sdk/platform-tools:/opt/android-sdk/emulator:/home/peppy/.cargo/bin:$PATH
USER ${DEV_UID}:${DEV_GID}
RUN curl --fail --location --retry 3 https://sh.rustup.rs -o /tmp/rustup.sh \
 && sh /tmp/rustup.sh -y --default-toolchain "$RUST_VERSION" --profile minimal \
 && rustup target add aarch64-linux-android x86_64-linux-android

# Licenses are intentionally not accepted while building the image.  Set
# PEPPY_ACCEPT_ANDROID_LICENSES=1 for an explicit builder invocation.
COPY --chown=${DEV_UID}:${DEV_GID} infra/dev/android-container-run.sh /usr/local/bin/run
RUN chmod 0755 /usr/local/bin/run
WORKDIR /workspace
CMD ["bash"]
