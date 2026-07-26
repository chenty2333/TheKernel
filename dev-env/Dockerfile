ARG DEBIAN_IMAGE=debian:bookworm-slim
ARG RUST_TOOLCHAIN=nightly-2026-06-07
ARG CARGO_AXPLAT_VERSION=0.3.0
ARG QEMU_VERSION=9.2.1
ARG QEMU_SHA256=72874fe9c395ced0c7fd7c22c43744072697f7ee1926a72237bd81784b2faf62
ARG LOONGARCH64_MUSL_TOOLCHAIN_URL=https://github.com/LoongsonLab/oscomp-toolchains-for-oskernel/releases/download/loongarch64-linux-musl-cross-gcc-13.2.0/loongarch64-linux-musl-cross.tgz
ARG LOONGARCH64_MUSL_TOOLCHAIN_SHA256=72c4f48c955345356bd05766129bc47ee03bdfdeebec4f47e8082b2ecc45e5fa

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
      --target-list="riscv64-softmmu,loongarch64-softmmu,aarch64-softmmu,x86_64-softmmu" \
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
ARG LOONGARCH64_MUSL_TOOLCHAIN_URL
ARG LOONGARCH64_MUSL_TOOLCHAIN_SHA256
ARG RUSTUP_HOME=/usr/local/rustup
ARG CARGO_HOME=/usr/local/cargo

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get -o Acquire::Retries=5 update \
 && apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
      bash-completion \
      binutils \
      binutils-aarch64-linux-gnu \
      binutils-riscv64-linux-gnu \
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
      gcc-aarch64-linux-gnu \
      gcc-riscv64-linux-gnu \
      gdb \
      git \
      less \
      libc6-dev-arm64-cross \
      libc6-dev-riscv64-cross \
      libclang-dev \
      libfdt1 \
      libglib2.0-0 \
      libpixman-1-0 \
      libslirp0 \
      make \
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

RUN set -eux; \
    curl --http1.1 -L --fail --retry 5 --retry-all-errors --continue-at - \
      --output /tmp/loongarch64-linux-musl-cross.tgz \
      "${LOONGARCH64_MUSL_TOOLCHAIN_URL}"; \
    echo "${LOONGARCH64_MUSL_TOOLCHAIN_SHA256}  /tmp/loongarch64-linux-musl-cross.tgz" | sha256sum -c -; \
    tar -C /opt -xf /tmp/loongarch64-linux-musl-cross.tgz; \
    rm -f /tmp/loongarch64-linux-musl-cross.tgz

RUN set -eux; \
    mkdir -p /usr/local/cross-compat/bin; \
    for tool in addr2line ar as c++ c++filt cc cpp g++ gcc gprof ld ld.bfd nm objcopy objdump ranlib readelf size strings strip; do \
      if [ -x "/usr/bin/aarch64-linux-gnu-${tool}" ]; then \
        ln -sf "/usr/bin/aarch64-linux-gnu-${tool}" "/usr/local/cross-compat/bin/aarch64-linux-musl-${tool}"; \
      fi; \
      if [ -x "/usr/bin/riscv64-linux-gnu-${tool}" ]; then \
        ln -sf "/usr/bin/riscv64-linux-gnu-${tool}" "/usr/local/cross-compat/bin/riscv64-linux-musl-${tool}"; \
      fi; \
      if [ -x "/usr/bin/x86_64-linux-gnu-${tool}" ]; then \
        ln -sf "/usr/bin/x86_64-linux-gnu-${tool}" "/usr/local/cross-compat/bin/x86_64-linux-musl-${tool}"; \
      elif [ -x "/usr/bin/${tool}" ]; then \
        ln -sf "/usr/bin/${tool}" "/usr/local/cross-compat/bin/x86_64-linux-musl-${tool}"; \
      fi; \
      if [ -x "/usr/local/cross-compat/bin/riscv64-linux-musl-${tool}" ]; then \
        ln -sf "/usr/local/cross-compat/bin/riscv64-linux-musl-${tool}" "/usr/local/cross-compat/bin/riscv64-linux-gnu-${tool}"; \
      fi; \
      if [ -x "/opt/loongarch64-linux-musl-cross/bin/loongarch64-linux-musl-${tool}" ]; then \
        ln -sf "/opt/loongarch64-linux-musl-cross/bin/loongarch64-linux-musl-${tool}" "/usr/local/cross-compat/bin/loongarch64-linux-gnu-${tool}"; \
      fi; \
    done

RUN export RUSTUP_HOME="${RUSTUP_HOME}" CARGO_HOME="${CARGO_HOME}"; \
    mkdir -p "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
 && "${CARGO_HOME}/bin/rustup" component add rust-src llvm-tools rustfmt clippy \
      --toolchain "${RUST_TOOLCHAIN}" \
 && "${CARGO_HOME}/bin/rustup" target add \
      x86_64-unknown-none \
      riscv64gc-unknown-none-elf \
      aarch64-unknown-none-softfloat \
      loongarch64-unknown-none-softfloat \
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
ENV PATH=${CARGO_HOME}/bin:/usr/local/cross-compat/bin:/opt/loongarch64-linux-musl-cross/bin:/opt/qemu-${QEMU_VERSION}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN printf '%s\n' \
    'export PATH=/usr/local/cargo/bin:/usr/local/cross-compat/bin:/opt/loongarch64-linux-musl-cross/bin:/opt/qemu-'${QEMU_VERSION}'/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' \
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
