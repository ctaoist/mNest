FROM node:24-bookworm-slim AS frontend
WORKDIR /build

COPY web/package.json web/package-lock.json ./web/
RUN --mount=type=cache,target=/root/.npm \
    npm --prefix web ci --no-audit --no-fund

COPY vite.config.ts ./
COPY web ./web
RUN npm --prefix web run build

FROM debian:bookworm-slim AS media-tools

ARG FFMPEG_VERSION=5.1.7
ARG FFMPEG_SHA256=27d87965c5b0ab857a0092aeb9f55d975becb7126d83aefe39ae24102492180b
ARG OPUS_VERSION=1.5.2
ARG OPUS_SHA256=65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1
ARG CHROMAPRINT_VERSION=1.5.1
ARG CHROMAPRINT_SHA256=a1aad8fa3b8b18b78d3755b3767faff9abb67242e01b478ec9a64e190f335e1c

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        libmp3lame-dev \
        libogg-dev \
        libssl-dev \
        libvorbis-dev \
        nasm \
        patch \
        patchelf \
        perl \
        pkg-config \
        wget \
        xz-utils \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

RUN --mount=type=cache,target=/var/cache/media-sources,sharing=locked \
    archive="/var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz"; \
    if ! echo "${OPUS_SHA256}  ${archive}" | sha256sum -c - >/dev/null 2>&1; then \
        rm -f "${archive}.tmp"; \
        wget --https-only --tries=3 --timeout=30 -O "${archive}.tmp" \
            "https://downloads.xiph.org/releases/opus/opus-${OPUS_VERSION}.tar.gz"; \
        echo "${OPUS_SHA256}  ${archive}.tmp" | sha256sum -c -; \
        mv "${archive}.tmp" "${archive}"; \
    fi \
    && echo "${OPUS_SHA256}  ${archive}" | sha256sum -c - \
    && tar -xzf "/var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz" \
    && cd "opus-${OPUS_VERSION}" \
    && ./configure \
        --prefix=/opt/media \
        --disable-doc \
        --disable-extra-programs \
        --enable-shared \
        --disable-static \
        --with-pic \
    && make -j"$(getconf _NPROCESSORS_ONLN)" \
    && make install

ENV PKG_CONFIG_PATH=/opt/media/lib/pkgconfig

RUN --mount=type=cache,target=/var/cache/media-sources,sharing=locked \
    archive="/var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz"; \
    if ! echo "${FFMPEG_SHA256}  ${archive}" | sha256sum -c - >/dev/null 2>&1; then \
        rm -f "${archive}.tmp"; \
        wget --https-only --tries=3 --timeout=30 -O "${archive}.tmp" \
            "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz"; \
        echo "${FFMPEG_SHA256}  ${archive}.tmp" | sha256sum -c -; \
        mv "${archive}.tmp" "${archive}"; \
    fi \
    && echo "${FFMPEG_SHA256}  ${archive}" | sha256sum -c - \
    && tar -xJf "/var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && cd "ffmpeg-${FFMPEG_VERSION}" \
    && ./configure \
        --prefix=/opt/media \
        --disable-autodetect \
        --disable-avdevice \
        --disable-debug \
        --disable-doc \
        --disable-encoders \
        --disable-programs \
        --disable-filters \
        --disable-muxers \
        --disable-postproc \
        --enable-shared \
        --disable-swscale \
        --enable-libmp3lame \
        --enable-libopus \
        --enable-libvorbis \
        --enable-encoder=aac,flac,libmp3lame,libopus,libvorbis,pcm_f32le \
        --enable-filter=aformat,anull,aresample,asetpts,atrim \
        --enable-muxer=adts,flac,mp3,ogg,opus,pcm_f32le \
        --enable-openssl \
        --enable-pthreads \
        --enable-small \
        --disable-static \
        --enable-version3 \
        --enable-zlib \
        --extra-cflags="-Os -fPIC -I/opt/media/include" \
        --extra-ldflags="-L/opt/media/lib" \
        --extra-libs="-lpthread -lm" \
    && make -j"$(getconf _NPROCESSORS_ONLN)" \
    && make install

COPY docker/chromaprint /tmp/chromaprint-patches
COPY docker/MEDIA-SOURCES.md /tmp/MEDIA-SOURCES.md

# Chromaprint 1.5.1 predates FFmpeg 5's send/receive decoding API. The patch
# below is the same compatibility fix shipped by Debian Bookworm.
RUN --mount=type=cache,target=/var/cache/media-sources,sharing=locked \
    archive="/var/cache/media-sources/chromaprint-${CHROMAPRINT_VERSION}.tar.gz"; \
    if ! echo "${CHROMAPRINT_SHA256}  ${archive}" | sha256sum -c - >/dev/null 2>&1; then \
        rm -f "${archive}.tmp"; \
        wget --https-only --tries=3 --timeout=30 -O "${archive}.tmp" \
            "https://github.com/acoustid/chromaprint/archive/refs/tags/v${CHROMAPRINT_VERSION}.tar.gz"; \
        echo "${CHROMAPRINT_SHA256}  ${archive}.tmp" | sha256sum -c -; \
        mv "${archive}.tmp" "${archive}"; \
    fi \
    && echo "${CHROMAPRINT_SHA256}  ${archive}" | sha256sum -c - \
    && tar -xzf "/var/cache/media-sources/chromaprint-${CHROMAPRINT_VERSION}.tar.gz" \
    && cd "chromaprint-${CHROMAPRINT_VERSION}" \
    && patch -p1 < /tmp/chromaprint-patches/0001-port-to-ffmpeg-5.patch \
    && cd /build \
    && cmake \
        -S "chromaprint-${CHROMAPRINT_VERSION}" \
        -B chromaprint-build \
        -DAUDIO_PROCESSOR_LIB=swresample \
        -DBUILD_SHARED_LIBS=OFF \
        -DBUILD_TESTS=OFF \
        -DBUILD_TOOLS=ON \
        -DCMAKE_BUILD_TYPE=MinSizeRel \
        -DCMAKE_INSTALL_PREFIX=/opt/media \
        -DFFMPEG_ROOT=/opt/media \
        -DFFT_LIB=kissfft \
    && cmake --build chromaprint-build --parallel "$(getconf _NPROCESSORS_ONLN)" \
    && cmake --install chromaprint-build \
    && strip /opt/media/bin/fpcalc \
    && mkdir -p /opt/media/licenses \
    && cp "ffmpeg-${FFMPEG_VERSION}/COPYING.LGPLv3" /opt/media/licenses/FFmpeg-LGPLv3 \
    && cp "opus-${OPUS_VERSION}/COPYING" /opt/media/licenses/Opus-COPYING \
    && cp "chromaprint-${CHROMAPRINT_VERSION}/LICENSE.md" /opt/media/licenses/Chromaprint-LICENSE.md \
    && cp /tmp/MEDIA-SOURCES.md /opt/media/licenses/MEDIA-SOURCES.md \
    && cp /usr/share/doc/libmp3lame-dev/copyright /opt/media/licenses/LAME-copyright \
    && cp /usr/share/doc/libvorbis-dev/copyright /opt/media/licenses/Vorbis-copyright \
    && cp /usr/share/doc/libogg-dev/copyright /opt/media/licenses/Ogg-copyright \
    && triplet="$(dpkg-architecture -qDEB_HOST_MULTIARCH)" \
    && mkdir -p /opt/media/runtime \
    && cp -a /opt/media/lib/libavcodec.so* /opt/media/runtime/ \
    && cp -a /opt/media/lib/libavformat.so* /opt/media/runtime/ \
    && cp -a /opt/media/lib/libavutil.so* /opt/media/runtime/ \
    && cp -a /opt/media/lib/libswresample.so* /opt/media/runtime/ \
    && cp -a /opt/media/lib/libopus.so* /opt/media/runtime/ \
    && cp -a "/usr/lib/${triplet}"/libmp3lame.so.0* /opt/media/runtime/ \
    && cp -a "/usr/lib/${triplet}"/libogg.so.0* /opt/media/runtime/ \
    && cp -a "/usr/lib/${triplet}"/libvorbis.so.0* /opt/media/runtime/ \
    && cp -a "/usr/lib/${triplet}"/libvorbisenc.so.2* /opt/media/runtime/ \
    && find /opt/media/runtime -type f -name '*.so*' -exec patchelf --set-rpath '$ORIGIN' {} \; \
    && patchelf --set-rpath '$ORIGIN/../lib' /opt/media/bin/fpcalc

FROM rust:1.95-bookworm AS backend
WORKDIR /build

ARG APP_VERSION=0.1.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY --from=frontend /build/web/dist ./web/dist
COPY --from=media-tools /opt/media /opt/media

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    PKG_CONFIG_PATH=/opt/media/lib/pkgconfig \
    LD_LIBRARY_PATH=/opt/media/lib:/opt/media/runtime \
    RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/lib' \
    MNEST_BUILD_VERSION="${APP_VERSION}" cargo test --locked --release --lib media::tests \
    && PKG_CONFIG_PATH=/opt/media/lib/pkgconfig \
        LD_LIBRARY_PATH=/opt/media/lib:/opt/media/runtime \
        RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/lib' \
        MNEST_BUILD_VERSION="${APP_VERSION}" cargo build --locked --release \
    && cp /build/target/release/mNest /build/mNest

FROM scratch AS release-bundle
COPY --from=backend /build/mNest /mNest
COPY --from=media-tools /opt/media/runtime /lib
COPY --from=media-tools /opt/media/licenses /licenses
COPY config.example.yaml LICENSE README.md /

FROM debian:bookworm-slim AS runtime

ARG APP_VERSION=0.1.0
LABEL org.opencontainers.image.title="mNest" \
      org.opencontainers.image.description="Self-hosted music library, tag scraper and OpenSubsonic server" \
      org.opencontainers.image.version="${APP_VERSION}" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
        libtagc0 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/mnest
COPY --from=backend /build/mNest /opt/mnest/mNest
COPY --from=media-tools /opt/media/runtime /opt/mnest/lib
COPY --from=media-tools /opt/media/bin/fpcalc /usr/bin/fpcalc
COPY --from=media-tools /opt/media/licenses /usr/share/licenses/mnest-media-tools
COPY config.example.yaml LICENSE ./

RUN mkdir -p /data /music

ENV MNEST_CONFIG=/data/config.yaml \
    MNEST_HEALTH_URL=http://127.0.0.1:4535/health \
    LD_LIBRARY_PATH=/opt/mnest/lib

VOLUME ["/data", "/music"]
EXPOSE 4535
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error "$MNEST_HEALTH_URL" >/dev/null || exit 1

ENTRYPOINT ["/opt/mnest/mNest"]
