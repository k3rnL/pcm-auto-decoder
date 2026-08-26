FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie
LABEL authors="Erwan Daniel"

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        binutils \
        clang \
        file \
        libavcodec-dev \
        libavformat-dev \
        libavutil-dev \
        libclang-dev \
        libpipewire-0.3-dev \
        libswresample-dev \
        pkg-config \
        python3 \
    && rm -rf /var/lib/apt/lists/*
