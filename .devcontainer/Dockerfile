FROM mcr.microsoft.com/devcontainers/rust:2-1-bullseye
LABEL authors="Erwan Daniel"

RUN apt update -y && apt install -y libpulse-dev ffmpeg

ENTRYPOINT ["top", "-b"]