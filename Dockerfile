FROM rust:1.59 as builder
WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

FROM debian:buster-slim
COPY --from=builder /usr/src/app/target/release/webhook-mailbox /usr/local/bin/webhook-mailbox
ENV ROCKET_ADDRESS="0.0.0.0"
CMD ["webhook-mailbox"]
