FROM rust:1.75-alpine AS builder
WORKDIR /app

RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

COPY . .

RUN cargo build --release

FROM alpine:3.19
WORKDIR /app

RUN apk add --no-cache \
    ffmpeg \
    ca-certificates \
    tzdata \
    libc6-compat

RUN cp /usr/share/zoneinfo/Asia/Shanghai /etc/localtime && \
    echo "Asia/Shanghai" > /etc/timezone

COPY --from=builder /app/target/release/stream_hub .

RUN mkdir -p /dev/shm/stream_hub

EXPOSE 3000

CMD ["./stream_hub"]