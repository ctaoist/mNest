# Bundled media components

Official mNest images and Linux release archives dynamically link the following components.
The exact build options are recorded in the repository `Dockerfile`.

| Component | Version | Source | SHA-256 |
| --- | --- | --- | --- |
| FFmpeg | 5.1.7 | https://ffmpeg.org/releases/ffmpeg-5.1.7.tar.xz | `27d87965c5b0ab857a0092aeb9f55d975becb7126d83aefe39ae24102492180b` |
| Opus | 1.5.2 | https://downloads.xiph.org/releases/opus/opus-1.5.2.tar.gz | `65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1` |
| Chromaprint | 1.5.1 | https://github.com/acoustid/chromaprint/archive/refs/tags/v1.5.1.tar.gz | `a1aad8fa3b8b18b78d3755b3767faff9abb67242e01b478ec9a64e190f335e1c` |

LAME, libogg and libvorbis are the unmodified Debian Bookworm packages identified in their
included Debian copyright files. Recipients may replace the shared libraries in `lib/` with
ABI-compatible builds; the application locates them through `$ORIGIN/lib`.
