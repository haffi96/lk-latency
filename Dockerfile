# syntax=docker/dockerfile:1.7
#
# Two targets, one Linux build:
#   binary  - builds lk-latency on Ubuntu 22.04 (glibc 2.35) and exports only the
#             binary. Release tarballs ship exactly this file.
#   image   - the runtime image; copies the already-built binary from
#             dist/linux-<arch>/ so Docker and native Linux run identical bytes.
#
#   scripts/build-linux.sh amd64     # -> dist/linux-amd64/lk-latency
#   docker build --target image -t lk-latency .
#
# Credentials are never part of either target; pass them at run time.

FROM ubuntu:22.04 AS build
ARG LLVM_VERSION=21.1.8
ARG LK_LATENCY_COMMIT=unknown
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl xz-utils git build-essential pkg-config lld \
    libssl-dev libx11-dev libgl1-mesa-dev libxext-dev libdrm-dev libgbm-dev \
    libxfixes-dev libxdamage-dev libxrandr-dev libxcomposite-dev libglib2.0-dev \
    libjpeg-turbo8-dev libudev-dev libv4l-dev libva-dev libxkbcommon-dev libwayland-dev \
    && if [ "$(uname -m)" = x86_64 ]; then apt-get install -y --no-install-recommends nvidia-cuda-dev; fi \
    && rm -rf /var/lib/apt/lists/*
# Static libjpeg from a private prefix: pkg-config won't link statically from /usr,
# and distros ship different libjpeg sonames (.so.8 vs .so.62).
RUN mkdir -p /opt/jpeg/lib/pkgconfig \
    && cp "$(dirname "$(find /usr/lib -name libjpeg.a | head -1)")/libjpeg.a" /opt/jpeg/lib/ \
    && printf 'libdir=/opt/jpeg/lib\nincludedir=/usr/include\nName: libjpeg\nDescription: static libjpeg\nVersion: 2.1\nLibs: -L${libdir} -ljpeg\nCflags: -I${includedir}\n' \
       > /opt/jpeg/lib/pkgconfig/libjpeg.pc
COPY scripts/install-clang.sh /tmp/install-clang.sh
RUN LLVM_VERSION=$LLVM_VERSION LLVM_ROOT=/opt/llvm bash /tmp/install-clang.sh >/dev/null
# CUDA_HOME=/usr finds Ubuntu's cuda.h (NVENC); libva-dev enables VAAPI. Both are
# dlopened at run time, so the binary still runs on machines without them.
# LIBJPEG_STATIC links libjpeg statically so the binary does not need it installed.
ENV PATH=/opt/llvm/bin:/root/.cargo/bin:$PATH CC=clang CXX=clang++ LIBCLANG_PATH=/opt/llvm/lib \
    CUDA_HOME=/usr LIBJPEG_STATIC=1 PKG_CONFIG_PATH=/opt/jpeg/lib/pkgconfig
WORKDIR /src
COPY rust-toolchain.toml ./
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
    && rustup toolchain install
COPY . .
ARG TARGETARCH
RUN --mount=type=cache,id=lk-latency-cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=lk-latency-cargo-git,target=/root/.cargo/git \
    --mount=type=cache,id=lk-latency-target-$TARGETARCH,target=/src/target \
    LK_LATENCY_COMMIT=$LK_LATENCY_COMMIT cargo build --locked --release \
    && mkdir -p /out && cp target/release/lk-latency /out/ \
    && /out/lk-latency publisher --list-encoders

FROM scratch AS binary
COPY --from=build /out/lk-latency /lk-latency

FROM ubuntu:22.04 AS image
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates iptables libstdc++6 libudev1 libv4l-0 libva2 libva-drm2 \
    libx11-6 libxrandr2 libxi6 libxfixes3 libxdamage1 libxcomposite1 libxext6 libxkbcommon0 \
    libgbm1 libdrm2 libegl1 libgl1 libvulkan1 libdbus-1-3 libglib2.0-0 \
    && rm -rf /var/lib/apt/lists/*
ARG TARGETARCH
COPY dist/linux-${TARGETARCH}/lk-latency /usr/local/bin/lk-latency
# Fail the build, not the first run, if a shared library is missing.
RUN ! ldd /usr/local/bin/lk-latency | grep "not found" && lk-latency version
ENV LK_LATENCY_OUTPUT=/results
VOLUME /results
WORKDIR /results
ENTRYPOINT ["lk-latency"]
CMD ["--help"]
