FROM mcr.microsoft.com/devcontainers/rust:2-1-bullseye
LABEL authors="Erwan Daniel"

RUN apt update -y && apt install -y libpulse-dev ffmpeg

COPY entrypoint.sh /entrypoint.sh

ENTRYPOINT ["bash", "/entrypoint.sh"]