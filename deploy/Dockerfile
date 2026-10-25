# syntax=docker/dockerfile:1.7

# =============================================================================
# mediaopt - deployment image
#
# WHY THIS IMAGE BUILDS TWO LIBRARIES FROM SOURCE
# ------------------------------------------------
# The entire point of mediaopt is JPEG density at a fixed quality. libvips only
# reaches that density when it is linked against *mozjpeg*; Debian's libvips is
# linked against libjpeg-turbo, which produces files 3.6-9.3% larger at the same
# SSIMULACRA2 score. Debian does not package mozjpeg at all, so mozjpeg and
# libvips are both compiled here. That is the only reason this Dockerfile is
# not fifteen lines long.
#
# ENGINE BINARIES AND WHERE EACH ONE COMES FROM
# ---------------------------------------------
#   vips, vipsheader  built from libvips source below, linked against the
#                     mozjpeg built below. vipsheader is required by
#                     `mediaopt verify` (src/engine.rs looks for it next to the
#                     vips binary, then on PATH).
#   jpegtran          from that same mozjpeg build. Debian's jpegtran is
#                     libjpeg-turbo, i.e. the thing we must not use.
#   pngquant          Debian 13 package (2.18.0). It is packaged and linked
#                     against libimagequant; nothing to build.
#   ssimulacra2       Debian 13 package `libjxl-devtools` (0.11.2). It is NOT
#                     in `libjxl-tools`; `libjxl-devtools` is the only Debian
#                     package that ships the binary. Upstream libjxl publishes
#                     prebuilt tools only for linux-x86_64 (jxl-linux-x86_64-
#                     static.tar.lz), so using the Debian package is the only
#                     route that covers linux/arm64 as well without a second,
#                     slow libjxl source build. We copy just that one binary
#                     plus its shared-library closure out of the package.
#
# MULTI-ARCH
# ----------
#   docker buildx build \
#     --platform linux/amd64,linux/arm64 \
#     -f deploy/Dockerfile -t ghcr.io/hansvaneijsden/media-optimizer:dev .
# linux/amd64 is what the servers run; linux/arm64 is for an Apple Silicon Mac.
# Both are supported because both only need source builds of mozjpeg/libvips
# (Debian has packages for pngquant and ssimulacra2 on both architectures).
# NOTE: the arm64 build is slower than a typical image, because mozjpeg and
# libvips are compiled from source for arm64 too: roughly 5-10 minutes natively
# on an M-series Mac, and considerably longer on the amd64 release runner where
# arm64 has to run under QEMU emulation. The Rust build is cached separately, so
# editing src/ does not rebuild the engines.
#
# SIZE
# ----
# Target: < 120 MB. The runtime stage starts from debian:13-slim, drops the
# package manager and Perl (unusable with the read-only root filesystem in
# deploy/compose.example.yaml), and is then squashed into a single scratch
# layer. The squash matters: Docker counts deleted files from lower layers, so
# without it the image would report ~137 MB even though only 106 MB is used.
# There is no compiler, no apt cache and no source tree in the final image.
# Measured: ~87 MB uncompressed layer on linux/amd64 and ~111 MB on linux/arm64
# (what a Linux Docker engine reports), ~35 MB compressed (what a server pulls).
# See "SIZE CHECK" at the end.
#
# Why not Alpine: a musl base would shrink the uncompressed image, but it also
# means musl source builds of mozjpeg, libvips *and* libjxl (Debian's
# ssimulacra2 package is glibc-only) and it changes the encoder/musl runtime
# behaviour that the quality numbers in docs/ were measured on. Since Debian 13
# plus the squash below fits the budget, the byte count is not worth that risk.
# =============================================================================

ARG RUST_VERSION=1.99.1
ARG DEBIAN_SUITE=13

# -----------------------------------------------------------------------------
# Stage 1: the Rust binary.
#
# Kept separate from the engine stage so a source change only re-runs cargo,
# and an engine change only re-runs the C/C++ builds.
# -----------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-slim-trixie AS rust-build

WORKDIR /build

# rusqlite is built with the `bundled` feature, so a C compiler is required.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

# --locked keeps the image on the exact dependency set in Cargo.lock. Note
# .gitignore ignores *.lock but explicitly un-ignores Cargo.lock for exactly
# this reason.
RUN cargo build --release --locked --bin mediaopt

# -----------------------------------------------------------------------------
# Stage 2: the engines. Everything here is thrown away except /out.
# -----------------------------------------------------------------------------
FROM debian:${DEBIAN_SUITE}-slim AS builder

ARG TARGETARCH

ARG MOZJPEG_VERSION=4.1.5
ARG MOZJPEG_SHA256=9fcbb7171f6ac383f5b391175d6fb3acde5e64c4c4727274eade84ed0998fcc1
ARG LIBVIPS_VERSION=8.18.7
ARG LIBVIPS_SHA256=5baaead3b0bb20ffdb9e9ff09aa9fda08620923df77b63b436654cb5e0b3bf94

ENV DEBIAN_FRONTEND=noninteractive \
    PKG_CONFIG_PATH=/usr/local/lib/pkgconfig

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      cmake \
      curl \
      libexpat1-dev \
      libglib2.0-dev \
      libimagequant-dev \
      libjxl-devtools \
      libpng-dev \
      libwebp-dev \
      meson \
      ninja-build \
      pkg-config \
      pngquant \
      tini \
      xz-utils \
      zlib1g-dev \
 && if [ "$TARGETARCH" = "amd64" ]; then \
      # nasm is only needed for the x86 SIMD assembly. Debian has no nasm on
      # arm64 and mozjpeg does not need it there (its arm64 NEON code uses gas).
      apt-get install -y --no-install-recommends nasm; \
    fi \
 && rm -rf /var/lib/apt/lists/*

# -----------------------------------------------------------------------------
# mozjpeg 4.1.5 -> /usr/local
#
# Installed into /usr/local together with libvips, so there is exactly one
# pkg-config prefix and no risk of picking up the distro's libjpeg.so.62.
# ENABLE_STATIC=OFF and WITH_TURBOJPEG=OFF keep the build (and the installed
# tree) small; we only need libjpeg.so.62 and jpegtran.
# -----------------------------------------------------------------------------
RUN set -eux; \
    curl -fsSL -o /tmp/mozjpeg.tar.gz \
      "https://codeload.github.com/mozilla/mozjpeg/tar.gz/refs/tags/v${MOZJPEG_VERSION}"; \
    echo "${MOZJPEG_SHA256}  /tmp/mozjpeg.tar.gz" | sha256sum -c -; \
    mkdir -p /tmp/mozjpeg-src; \
    tar -xzf /tmp/mozjpeg.tar.gz -C /tmp/mozjpeg-src --strip-components=1; \
    cmake -S /tmp/mozjpeg-src -B /tmp/mozjpeg-build \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DENABLE_SHARED=TRUE \
        -DENABLE_STATIC=FALSE \
        -DPNG_SUPPORTED=FALSE \
        -DWITH_TURBOJPEG=FALSE \
        -DWITH_SIMD=TRUE; \
    cmake --build /tmp/mozjpeg-build --parallel "$(nproc)"; \
    cmake --install /tmp/mozjpeg-build; \
    rm -rf /tmp/mozjpeg-*; \
    ldconfig; \
    test -f /usr/local/lib/libjpeg.so.62; \
    test -x /usr/local/bin/jpegtran; \
    /usr/local/bin/jpegtran -version 2>&1 | head -n 3

# -----------------------------------------------------------------------------
# libvips 8.18.7 -> /usr/local, against the mozjpeg above.
#
# jpeg, png, webp and imagequant are the only loaders/savers this tool uses;
# everything else is disabled to keep the runtime shared-library closure small.
# imagequant is what makes `pngsave --palette` work (src/png.rs relies on it for
# the palette path). The -Dc_args/-Dcpp_args include path is belt and braces on
# top of PKG_CONFIG_PATH, and the rpath makes the runner independent of
# ld.so.conf ordering: vips always loads /usr/local/lib/libjpeg.so.62.
#
# -Dmodules=disabled matters: in 8.18 modules default to *enabled*, which would
# scatter loaders into /usr/local/lib/vips-modules-8.18 and drag in more libs.
# --libdir=lib is not cosmetic: Debian's meson defaults libdir to
# lib/<multiarch> even with --prefix=/usr/local, which would put libvips in
# /usr/local/lib/aarch64-linux-gnu and split the prefix away from mozjpeg.
# -----------------------------------------------------------------------------
RUN set -eux; \
    curl -fsSL -o /tmp/vips.tar.xz \
      "https://github.com/libvips/libvips/releases/download/v${LIBVIPS_VERSION}/vips-${LIBVIPS_VERSION}.tar.xz"; \
    echo "${LIBVIPS_SHA256}  /tmp/vips.tar.xz" | sha256sum -c -; \
    mkdir -p /tmp/vips-src; \
    tar -xJf /tmp/vips.tar.xz -C /tmp/vips-src --strip-components=1; \
    meson setup /tmp/vips-build /tmp/vips-src \
        --buildtype=release \
        --prefix=/usr/local \
        --libdir=lib \
        -Djpeg=enabled \
        -Dpng=enabled \
        -Dwebp=enabled \
        -Dimagequant=enabled \
        -Dspng=disabled \
        -Dtiff=disabled \
        -Dheif=disabled \
        -Dheif-module=disabled \
        -Djpeg-xl=disabled \
        -Djpeg-xl-module=disabled \
        -Duhdr=disabled \
        -Dmagick=disabled \
        -Dopenexr=disabled \
        -Dopenjpeg=disabled \
        -Dpoppler=disabled \
        -Dpdfium=disabled \
        -Drsvg=disabled \
        -Dcfitsio=disabled \
        -Dfftw=disabled \
        -Dfontconfig=disabled \
        -Dpangocairo=disabled \
        -Dmatio=disabled \
        -Dopenslide=disabled \
        -Dopenslide-module=disabled \
        -Dorc=disabled \
        -Dhighway=disabled \
        -Dnsgif=false \
        -Dppm=false \
        -Danalyze=false \
        -Dradiance=false \
        -Dlcms=disabled \
        -Dexif=disabled \
        -Darchive=disabled \
        -Dquantizr=disabled \
        -Dcgif=disabled \
        -Ddeprecated=false \
        -Dmodules=disabled \
        -Dintrospection=disabled \
        -Dvapi=false \
        -Dexamples=false \
        -Dcplusplus=false \
        -Ddocs=false \
        -Dc_args="-I/usr/local/include" \
        -Dcpp_args="-I/usr/local/include" \
        -Dc_link_args="-L/usr/local/lib -Wl,-rpath,/usr/local/lib" \
        -Dcpp_link_args="-L/usr/local/lib -Wl,-rpath,/usr/local/lib"; \
    ninja -C /tmp/vips-build; \
    ninja -C /tmp/vips-build install; \
    rm -rf /tmp/vips-*; \
    ldconfig

# -----------------------------------------------------------------------------
# Hard gates. If any of these fail the image never gets built, which is the
# point: a silently libjpeg-turbo vips produces bigger files, not errors.
# -----------------------------------------------------------------------------
RUN set -eux; \
    vips --vips-config > /tmp/vips-config.txt; \
    cat /tmp/vips-config.txt; \
    # Must say true; if this says false, libvips has no JPEG support at all.
    grep -i 'JPEG load/save' /tmp/vips-config.txt | grep -q 'true'; \
    grep -i 'PNG load/save' /tmp/vips-config.txt | grep -q 'true'; \
    grep -i 'WebP load/save' /tmp/vips-config.txt | grep -q 'true'; \
    # imagequant is the only supported quantiser in this build (quantizr is off).
    grep -i 'image quantisation' /tmp/vips-config.txt | grep -q 'true'; \
    # The mozjpeg gate: the resolved libjpeg must be *ours*, not the distro's.
    # A multiarch path such as /lib/x86_64-linux-gnu/libjpeg.so.62 would be the
    # 3.6-9.3% regression this image exists to avoid.
    libjpeg_line="$(ldd "$(command -v vips)" | grep -i jpeg)"; \
    echo "$libjpeg_line"; \
    echo "$libjpeg_line" | grep -q '/usr/local/lib/libjpeg'; \
    if echo "$libjpeg_line" | grep -qE '/lib/(x86_64|aarch64)-linux-gnu/libjpeg'; then         echo "libvips resolved the distro libjpeg; this image would produce bigger files" >&2;         exit 1;     fi; \
    # Prove `pngsave --palette` really works, and that the CLI can read/write
    # through the JPEG loader (which is the mozjpeg build).
    vips black /tmp/probe.v 64 64; \
    vips pngsave /tmp/probe.v /tmp/probe.png --palette; \
    vips jpegsave /tmp/probe.v /tmp/probe.jpg; \
    test -s /tmp/probe.png; \
    test -s /tmp/probe.jpg; \
    test "$(vipsheader -f width /tmp/probe.jpg)" = "64"; \
    ssimulacra2 /tmp/probe.jpg /tmp/probe.jpg | grep -qE '^[0-9]'; \
    pngquant --version; \
    rm -f /tmp/probe.v /tmp/probe.png /tmp/probe.jpg /tmp/vips-config.txt

# -----------------------------------------------------------------------------
# Stage /out: exactly the binaries and shared libraries the runner needs.
#
# The shared-library closure is computed with ldd instead of hand-listing
# packages: libvips, libjxl and OpenEXR each pull in different transitive deps,
# and a hand-written apt list is a correctness bug waiting to happen. libc and
# friends are skipped because the runner base already has them.
# -----------------------------------------------------------------------------
RUN set -eux; \
    mkdir -p /out/bin /out/lib; \
    install -m 0755 \
        /usr/local/bin/vips \
        /usr/local/bin/vipsheader \
        /usr/local/bin/jpegtran \
        /out/bin/; \
    install -m 0755 /usr/bin/pngquant /usr/bin/ssimulacra2 /usr/bin/tini /out/bin/; \
    cp -a /usr/local/lib/libvips.so* /out/lib/; \
    cp -a /usr/local/lib/libjpeg.so* /out/lib/; \
    for bin in /out/bin/*; do \
        ldd "$bin" | awk '/=> \// {print $3}' | while IFS= read -r lib; do \
            name="$(basename "$lib")"; \
            case "$name" in \
                libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*|libgcc_s.so.*|libstdc++.so.*|ld-linux*) continue ;; \
            esac; \
            if [ ! -e "/out/lib/$name" ]; then cp -L "$lib" "/out/lib/$name"; fi; \
        done; \
    done; \
    # libvips is built with symbols; the distro libraries are already stripped.
    strip --strip-unneeded /out/lib/libvips.so* /out/lib/libjpeg.so*; \
    ls -l /out/bin /out/lib

# -----------------------------------------------------------------------------
# Stage 3: the runtime. Only what is needed to run, nothing else.
# -----------------------------------------------------------------------------
FROM debian:${DEBIAN_SUITE}-slim AS runtime

LABEL org.opencontainers.image.title="mediaopt" \
      org.opencontainers.image.description="Idempotent, quality-targeted image optimiser (libvips + mozjpeg)" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.source="https://github.com/HansVanEijsden/media-optimizer" \
      org.opencontainers.image.documentation="https://github.com/HansVanEijsden/media-optimizer"

# The scheduler (src/sched.rs) works in *local* time, so the container needs a
# timezone. debian:13-slim already ships tzdata; we only point it at Europe/
# Amsterdam and leave TZ overridable (`-e TZ=...`). Without this, MEDIAOPT_
# SCHEDULE=04:15 would fire at 04:15 UTC on a server in the Netherlands.
ENV DEBIAN_FRONTEND=noninteractive \
    MEDIAOPT_CONFIG=/etc/mediaopt/config.toml \
    TZ=Europe/Amsterdam

RUN set -eux; \
    ln -snf /usr/share/zoneinfo/Europe/Amsterdam /etc/localtime; \
    echo "Europe/Amsterdam" > /etc/timezone

COPY --from=builder /out/bin/ /usr/local/bin/
COPY --from=builder /out/lib/ /usr/local/lib/
COPY --from=rust-build /build/target/release/mediaopt /usr/local/bin/mediaopt

# The licenses travel with the image, so a pulled image is self-describing.
COPY LICENSE-MIT LICENSE-APACHE /usr/local/share/doc/mediaopt/

# Trim the base image. debian:13-slim is a *general purpose* root filesystem:
# it ships a package manager (apt, dpkg, perl) and their exclusive libraries,
# which a read-only-rootfs runtime cannot use anyway (the compose file mounts
# the root filesystem read-only, so `apt install` could never work). Removing
# those saves ~21 MB, which is the difference between missing and meeting the
# 120 MB budget. Shared libraries are deliberately kept: only the package
# manager's own programs and data, the Perl runtime and locale/completion data
# go away, and the RUN ends with `ldconfig` plus a check that no remaining
# executable in /usr/bin, /usr/sbin or /usr/local/bin has a missing library.
# /usr/share/doc and /usr/share/common-licenses stay: they carry the Debian
# packages' copyright/license texts.
RUN set -eux; \
    rm -rf \
        /usr/lib/*/perl-base \
        /usr/share/perl /usr/share/perl5 \
        /usr/bin/perl* \
        /usr/bin/apt* /usr/sbin/apt* /usr/lib/apt /etc/apt /var/lib/apt /var/log/apt \
        /usr/bin/dpkg* /usr/sbin/dpkg* /usr/lib/dpkg /var/lib/dpkg \
        /usr/lib/*/libapt-pkg.so* /usr/lib/*/libapt-private.so* \
        /usr/lib/*/libdb-5.3.so* /usr/lib/*/libsqlite3.so* /usr/bin/lslogins \
        /usr/share/locale /usr/lib/locale /usr/share/bash-completion /usr/share/gcc; \
    ldconfig; \
    broken=0; \
    for f in /usr/bin/* /usr/sbin/* /usr/local/bin/*; do \
        head -c 4 "$f" 2>/dev/null | grep -q ELF || continue; \
        if ldd "$f" 2>/dev/null | grep -q 'not found'; then \
            echo "trimming broke $f (missing shared library)" >&2; \
            broken=1; \
        fi; \
    done; \
    test "$broken" -eq 0

# ldconfig picks up /usr/local/lib (it is in the base image's ld.so.conf), so
# the binaries work through the normal loader with no LD_LIBRARY_PATH.
#
# The single RUN below creates the account, then verifies the *installed* image
# rather than trusting the builder. Everything it creates in /tmp is deleted in
# the same layer, so it costs no size. If mozjpeg's libjpeg.so.62 were ABI-
# incompatible with the libjpeg the Debian ssimulacra2 was linked against, the
# ssimulacra2 call on a JPEG would fail right here.
RUN set -eux; \
    ldconfig; \
    groupadd --gid 1000 mediaopt; \
    useradd --uid 1000 --gid 1000 --no-create-home \
        --home-dir /var/lib/mediaopt --shell /usr/sbin/nologin mediaopt; \
    install -d -o mediaopt -g mediaopt -m 0750 /var/lib/mediaopt; \
    install -d -m 0755 /etc/mediaopt; \
    libjpeg_line="$(ldd "$(command -v vips)" | grep -i jpeg)"; \
    echo "$libjpeg_line"; \
    echo "$libjpeg_line" | grep -q '/usr/local/lib/libjpeg'; \
    if echo "$libjpeg_line" | grep -qE '/lib/(x86_64|aarch64)-linux-gnu/libjpeg'; then         echo "runtime vips resolved the distro libjpeg" >&2;         exit 1;     fi; \
    vips --vips-config | grep -i 'JPEG load/save' | grep -q 'true'; \
    vips black /tmp/probe.v 64 64; \
    vips pngsave /tmp/probe.v /tmp/probe.png --palette; \
    vips jpegsave /tmp/probe.v /tmp/probe.jpg; \
    # JPEG in, JPEG out: exercises libjxl's libjpeg decoder against the mozjpeg
    # libjpeg we just installed.
    ssimulacra2 /tmp/probe.jpg /tmp/probe.jpg | grep -qE '^[0-9]'; \
    vipsheader -f width /tmp/probe.jpg | grep -qx 64; \
    jpegtran -version >/dev/null 2>&1; \
    pngquant --version >/dev/null 2>&1; \
    mediaopt --version; \
    rm -rf /tmp/probe.*

# -----------------------------------------------------------------------------
# Stage 4: squash the runtime stage into one layer.
#
# Why scratch and not `FROM debian:13-slim` directly: Docker's image size is the
# SUM of all layers, and files deleted in a later layer still count. The base
# debian:13-slim layer alone is ~110 MB, and the pruned files are part of it,
# so a `FROM debian:13-slim` image would report ~137 MB no matter how much the
# trim removes from the running filesystem. Copying the already-pruned,
# already-verified root filesystem into a scratch layer makes the image exactly
# the 106 MB that is actually used, which is how it fits the 120 MB budget.
#
# This is metadata-only from here on: scratch has no shell, so every RUN had to
# happen in the runtime stage above. The Debian userland (including /bin/sh) is
# present in the copied root filesystem, so `docker exec ... sh` still works at
# runtime; only the *build* cannot run commands in this stage.
# -----------------------------------------------------------------------------
FROM scratch AS image

LABEL org.opencontainers.image.title="mediaopt" \
      org.opencontainers.image.description="Idempotent, quality-targeted image optimiser (libvips + mozjpeg)" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.source="https://github.com/HansVanEijsden/media-optimizer" \
      org.opencontainers.image.documentation="https://github.com/HansVanEijsden/media-optimizer"

# scratch does not inherit the base image's environment or PATH.
ENV PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin \
    MEDIAOPT_CONFIG=/etc/mediaopt/config.toml \
    TZ=Europe/Amsterdam

# The single layer this image consists of.
COPY --from=runtime / /

# Health signal (compose overrides the timings). `mediaopt tools` alone is not
# enough: it parses the config and resolves the engines, but it still exits 0
# when a binary is missing (it only prints a WARNING). The second half is the
# check that matters for this image: libvips must start *and* still report JPEG
# support through the mozjpeg build. Both commands are cheap and touch no media,
# which matters because the scheduler legitimately sits idle for ~24 hours and
# a work-based probe would report a healthy container as unhealthy.
HEALTHCHECK --interval=5m --timeout=30s --start-period=2m --retries=3 \
    CMD mediaopt tools >/dev/null && vips --vips-config | grep -q 'JPEG load/save.*true'

USER mediaopt
WORKDIR /var/lib/mediaopt

# tini is PID 1: mediaopt installs SIGTERM/SIGINT handlers itself, but it is a
# subprocess of nothing, so without tini a `docker stop` signal could be
# delivered to a process that is not the direct child. tini reaps and forwards,
# which makes `stop_grace_period` in the compose file meaningful.
ENTRYPOINT ["/usr/local/bin/tini", "--", "/usr/local/bin/mediaopt"]
CMD ["schedule"]

STOPSIGNAL SIGTERM

# =============================================================================
# SIZE CHECK - measured by building both platforms on 2026-10-25.
# Reproduce with:
#
#   docker buildx build --platform linux/amd64 -f deploy/Dockerfile -t mediaopt:size .
#   # What `docker images` reports on a Linux engine (single layer, uncompressed):
#   docker image inspect mediaopt:size --format '{{.Size}}' \
#     | awk '{printf "%.1f MB\n", $1/1000000}'
#   # Bytes actually in the container filesystem:
#   docker run --rm --entrypoint du mediaopt:size -sb / | tail -1
#   # What a server downloads (compressed layer + manifest):
#   docker save mediaopt:size | gzip -1 | wc -c
#
#                          linux/amd64 (servers)   linux/arm64 (Apple Silicon)
#   uncompressed layer            ~87 MB                  111 MB
#     <- what a Linux Docker engine's `docker images` shows
#   rootfs (du -sb inside)        85.6 MB                 106 MB
#   compressed pull size          34.7 MB                 34.8 MB
#     <- what a server downloads from ghcr.io
#
# Both are below the 120 MB budget; arm64 is the larger one because the Debian
# arm64 base layer is larger than the amd64 one. Docker Desktop's `.Size` is not
# used for the budget: on a Mac it counts compressed blobs *and* unpacked
# snapshots (146 MB for the arm64 image here), which is not what a server
# deploys. Use the uncompressed layer or the compressed pull size.
#
# Reference points: the debian:13-slim base layer alone is ~80 MB (amd64) /
# ~100 MB (arm64) uncompressed, the engines plus their shared-library closure
# are ~20 MB, and the stripped mediaopt binary is ~6 MB. Any change that grows
# the image by more than a megabyte should come with a new measurement.
# =============================================================================
