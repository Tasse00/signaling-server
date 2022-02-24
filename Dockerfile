FROM rust:1.58-alpine3.15 AS builder

ARG HTTPS_PROXY
ARG HTTP_PROXY

WORKDIR /build

COPY ./ .

RUN rustup target add x86_64-unknown-linux-musl

RUN cargo update

RUN apk add --no-cache musl-dev

RUN cargo build --target=x86_64-unknown-linux-musl --release

FROM alpine:3.15

COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group

WORKDIR /app

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/signaling-server /app/signaling-server


EXPOSE 3003

RUN addgroup signaling_server && adduser signaling_server -D -G signaling_server
USER signaling_server:signaling_server

ENTRYPOINT ["./signaling-server"]
