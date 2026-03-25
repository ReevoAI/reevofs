FROM rust:1.93-slim-bookworm

RUN apt-get update && apt-get install -y \
    fuse3 libfuse3-dev pkg-config cmake g++ \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

RUN cargo build --features fuse 2>&1

CMD ["bash"]
