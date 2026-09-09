# 多阶段构建：容器内原生编译（避免 macOS→Linux 交叉编译依赖）。
# BoringSSL 需要 cmake、clang、go、perl。
FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake clang golang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
COPY src ./src
COPY tests ./tests
COPY xtask ./xtask
RUN cargo build --release --bin gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/gateway /usr/local/bin/gateway
EXPOSE 6180
CMD ["/usr/local/bin/gateway"]
