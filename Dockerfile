FROM node:24-bookworm-slim AS frontend
WORKDIR /build

COPY web/package.json web/package-lock.json ./web/
RUN --mount=type=cache,target=/root/.npm \
    npm --prefix web ci --no-audit --no-fund

COPY vite.config.ts ./
COPY web ./web
RUN npm --prefix web run build

FROM alpine:3.22 AS media-tools

ARG ALPINE_MIRROR=https://dl-cdn.alpinelinux.org/alpine
ARG FFMPEG_VERSION=5.1.7
ARG FFMPEG_SHA256=27d87965c5b0ab857a0092aeb9f55d975becb7126d83aefe39ae24102492180b
ARG OPUS_VERSION=1.5.2
ARG OPUS_SHA256=65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1
ARG CHROMAPRINT_VERSION=1.5.1
ARG CHROMAPRINT_SHA256=a1aad8fa3b8b18b78d3755b3767faff9abb67242e01b478ec9a64e190f335e1c

RUN sed -i "s|https://dl-cdn.alpinelinux.org/alpine|${ALPINE_MIRROR}|g" /etc/apk/repositories \
    && apk add --no-cache \
        build-base \
        cmake \
        lame-dev \
        libogg-dev \
        libogg-static \
        libvorbis-dev \
        libvorbis-static \
        linux-headers \
        nasm \
        openssl-dev \
        openssl-libs-static \
        perl \
        pkgconf \
        wget \
        xz \
        zlib-dev \
        zlib-static

WORKDIR /build

RUN --mount=type=cache,target=/var/cache/media-sources \
    test -f "/var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz" \
        || wget -q -O "/var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz" \
            "https://downloads.xiph.org/releases/opus/opus-${OPUS_VERSION}.tar.gz" \
    && echo "${OPUS_SHA256}  /var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz" | sha256sum -c - \
    && tar -xzf "/var/cache/media-sources/opus-${OPUS_VERSION}.tar.gz" \
    && cd "opus-${OPUS_VERSION}" \
    && ./configure \
        --prefix=/opt/media \
        --disable-doc \
        --disable-extra-programs \
        --disable-shared \
        --enable-static \
    && make -j"$(getconf _NPROCESSORS_ONLN)" \
    && make install

ENV PKG_CONFIG_PATH=/opt/media/lib/pkgconfig

RUN --mount=type=cache,target=/var/cache/media-sources \
    test -f "/var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
        || wget -q -O "/var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
            "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  /var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz" | sha256sum -c - \
    && tar -xJf "/var/cache/media-sources/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && cd "ffmpeg-${FFMPEG_VERSION}" \
    && ./configure \
        --prefix=/opt/media \
        --disable-autodetect \
        --disable-avdevice \
        --disable-debug \
        --disable-doc \
        --disable-encoders \
        --disable-ffplay \
        --disable-filters \
        --disable-muxers \
        --disable-postproc \
        --disable-shared \
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
        --enable-static \
        --enable-version3 \
        --enable-zlib \
        --extra-cflags="-Os -I/opt/media/include" \
        --extra-ldflags="-static -L/opt/media/lib" \
        --extra-libs="-lpthread -lm" \
        --pkg-config-flags=--static \
    && make -j"$(getconf _NPROCESSORS_ONLN)" \
    && make install \
    && /opt/media/bin/ffmpeg \
        -nostdin \
        -v error \
        -f f32le \
        -ar 8000 \
        -ac 1 \
        -i /dev/zero \
        -t 0.01 \
        -c:a pcm_f32le \
        -f f32le \
        -y /tmp/ffmpeg-f32le.raw \
    && test -s /tmp/ffmpeg-f32le.raw

COPY docker/chromaprint /tmp/chromaprint-patches

# Chromaprint 1.5.1 predates FFmpeg 5's send/receive decoding API. The patch
# below is the same compatibility fix shipped by Debian Bookworm.
RUN --mount=type=cache,target=/var/cache/media-sources \
    test -f "/var/cache/media-sources/chromaprint-${CHROMAPRINT_VERSION}.tar.gz" \
        || wget -q -O "/var/cache/media-sources/chromaprint-${CHROMAPRINT_VERSION}.tar.gz" \
            "https://github.com/acoustid/chromaprint/archive/refs/tags/v${CHROMAPRINT_VERSION}.tar.gz" \
    && echo "${CHROMAPRINT_SHA256}  /var/cache/media-sources/chromaprint-${CHROMAPRINT_VERSION}.tar.gz" | sha256sum -c - \
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
        -DCMAKE_CXX_STANDARD_LIBRARIES="-Wl,--end-group -lssl -lcrypto -lz -lmp3lame -lopus -lvorbisenc -lvorbis -logg -lpthread -latomic -ldl -lm" \
        -DCMAKE_EXE_LINKER_FLAGS="-static -L/opt/media/lib -Wl,--start-group" \
        -DCMAKE_INSTALL_PREFIX=/opt/media \
        -DFFMPEG_ROOT=/opt/media \
        -DFFT_LIB=kissfft \
    && cmake --build chromaprint-build --parallel "$(getconf _NPROCESSORS_ONLN)" \
    && cmake --install chromaprint-build \
    && strip /opt/media/bin/ffmpeg /opt/media/bin/ffprobe /opt/media/bin/fpcalc \
    && mkdir -p /opt/media/licenses \
    && cp "ffmpeg-${FFMPEG_VERSION}/COPYING.LGPLv3" /opt/media/licenses/FFmpeg-LGPLv3 \
    && cp "opus-${OPUS_VERSION}/COPYING" /opt/media/licenses/Opus-COPYING \
    && cp "chromaprint-${CHROMAPRINT_VERSION}/LICENSE.md" /opt/media/licenses/Chromaprint-LICENSE.md

FROM rust:1.95-bookworm AS backend
WORKDIR /build

ARG APP_VERSION=0.1.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY --from=frontend /build/web/dist ./web/dist

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    MNEST_BUILD_VERSION="${APP_VERSION}" cargo build --locked --release \
    && cp /build/target/release/mNest /build/mNest

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

WORKDIR /app
COPY --from=backend /build/mNest /usr/local/bin/mNest
COPY --from=media-tools /opt/media/bin/ffmpeg /usr/bin/ffmpeg
COPY --from=media-tools /opt/media/bin/ffprobe /usr/bin/ffprobe
COPY --from=media-tools /opt/media/bin/fpcalc /usr/bin/fpcalc
COPY --from=media-tools /opt/media/licenses /usr/share/licenses/mnest-media-tools
COPY config.example.yaml LICENSE ./

RUN mkdir -p /data /music

ENV MNEST_CONFIG=/data/config.yaml \
    MNEST_HEALTH_URL=http://127.0.0.1:4535/health

VOLUME ["/data", "/music"]
EXPOSE 4535
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error "$MNEST_HEALTH_URL" >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/mNest"]
