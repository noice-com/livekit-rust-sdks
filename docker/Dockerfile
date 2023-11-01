FROM ubuntu:22.04

RUN apt-get update && apt-get install -y cmake ninja-build build-essential git \
  curl python3 cpio python3-pkg-resources pkg-config libx11-dev libxext-dev libgl1-mesa-dev

RUN curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain 1.73.0 -y

ENV PATH="/root/.cargo/bin:${PATH}"

RUN rustup install stable && rustup default stable

WORKDIR /workspace
