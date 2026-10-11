FROM rust:1-trixie AS builder
WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim
COPY --from=builder /usr/src/app/target/release/webhook-mailbox /usr/local/bin/webhook-mailbox
CMD ["webhook-mailbox"]
