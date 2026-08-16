ARG DEBIAN_IMAGE=debian:bookworm-slim
ARG RUST_TOOLCHAIN=nightly
ARG CARGO_AXPLAT_VERSION=0.3.0
ARG QEMU_VERSION=9.2.1
ARG QEMU_SHA256=72874fe9c395ced0c7fd7c22c43744072697f7ee1926a72237bd81784b2faf62

FROM ${DEBIAN_IMAGE} AS qemu-builder

ARG DEBIAN_FRONTEND=noninteractive
ARG QEMU_VERSION
ARG QEMU_SHA256

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get -o Acquire::Retries=5 update \
 && apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
      build-essential \
      bzip2 \
      ca-certificates \
      curl \
      git \
      libfdt-dev \
      libglib2.0-dev \
      libpixman-1-dev \
      libslirp-dev \
      meson \
      ninja-build \
      pkg-config \
      python3 \
      python3-venv \
      xz-utils \
      zlib1g-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /tmp/qemu

RUN curl --http1.1 -L --fail --retry 5 --retry-all-errors --continue-at - \
      --output "qemu-${QEMU_VERSION}.tar.xz" \
      "https://download.qemu.org/qemu-${QEMU_VERSION}.tar.xz" \
 && echo "${QEMU_SHA256}  qemu-${QEMU_VERSION}.tar.xz" | sha256sum -c - \
 && tar -xf "qemu-${QEMU_VERSION}.tar.xz" \
 && cd "qemu-${QEMU_VERSION}" \
 && ./configure \
      --prefix="/opt/qemu-${QEMU_VERSION}" \
      --target-list="x86_64-softmmu" \
      --enable-slirp \
      --disable-docs \
      --disable-bsd-user \
      --disable-linux-user \
      --disable-werror \
 && make -j"$(nproc)" \
 && make install \
 && rm -rf /tmp/qemu

FROM ${DEBIAN_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_TOOLCHAIN
ARG CARGO_AXPLAT_VERSION
ARG QEMU_VERSION
ARG RUSTUP_HOME=/usr/local/rustup
ARG CARGO_HOME=/usr/local/cargo

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get -o Acquire::Retries=5 update \
 && apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
      bash-completion \
      binutils \
      build-essential \
      bzip2 \
      ca-certificates \
      clang \
      cmake \
      curl \
      dosfstools \
      e2fsprogs \
      fakeroot \
      file \
      gawk \
      gdb \
      git \
      grub-common \
      grub-efi-amd64-bin \
      less \
      libclang-dev \
      libfdt1 \
      libglib2.0-0 \
      libpixman-1-0 \
      libslirp0 \
      mtools \
      make \
      parted \
      pkg-config \
      python3 \
      python3-venv \
      rsync \
      gosu \
      u-boot-tools \
      unzip \
      xz-utils \
      zip \
 && rm -rf /var/lib/apt/lists/*

COPY --from=qemu-builder /opt/qemu-${QEMU_VERSION} /opt/qemu-${QEMU_VERSION}

RUN export RUSTUP_HOME="${RUSTUP_HOME}" CARGO_HOME="${CARGO_HOME}"; \
    mkdir -p "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
 && "${CARGO_HOME}/bin/rustup" component add rust-src llvm-tools rustfmt clippy \
      --toolchain "${RUST_TOOLCHAIN}" \
 && "${CARGO_HOME}/bin/rustup" target add \
      x86_64-unknown-none \
      --toolchain "${RUST_TOOLCHAIN}" \
 && "${CARGO_HOME}/bin/cargo" install --locked cargo-binutils axconfig-gen \
 && "${CARGO_HOME}/bin/cargo" install --locked --version "${CARGO_AXPLAT_VERSION}" cargo-axplat \
 && chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"

ENV LANG=C.UTF-8
ENV LC_ALL=C.UTF-8
ENV HOME=/home/thekernel
ENV RUSTUP_HOME=${RUSTUP_HOME}
ENV CARGO_HOME=${CARGO_HOME}
ENV RUSTUP_TOOLCHAIN=${RUST_TOOLCHAIN}
ENV THEKERNEL_QEMU_VERSION=${QEMU_VERSION}
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
ENV PATH=${CARGO_HOME}/bin:/opt/qemu-${QEMU_VERSION}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN printf '%s\n' \
    'export PATH=/usr/local/cargo/bin:/opt/qemu-'${QEMU_VERSION}'/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' \
    > /etc/profile.d/thekernel-dev-path.sh

COPY entrypoint.sh /usr/local/bin/thekernel-dev-entrypoint
COPY check-image.sh /usr/local/bin/thekernel-dev-check

RUN chmod +x /usr/local/bin/thekernel-dev-entrypoint /usr/local/bin/thekernel-dev-check \
 && mkdir -p /workspace /home/thekernel /opt/thekernel/rootfs \
 && groupadd -g 1000 thekernel || true \
 && useradd -m -u 1000 -g 1000 -s /bin/bash thekernel || true

WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/thekernel-dev-entrypoint"]
CMD ["/bin/bash"]
