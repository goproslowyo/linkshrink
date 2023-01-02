FROM rust:latest as builder

RUN apt update && apt install -y upx
RUN rustup update && rustup default nightly && \
    rustup target add x86_64-unknown-linux-musl && \
    rustup toolchain install nightly-x86_64-unknown-linux-musl
WORKDIR /app

COPY src/* src/
COPY Cargo.* ./

ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --target=x86_64-unknown-linux-musl
RUN upx --lzma --best /app/target/x86_64-unknown-linux-musl/release/linkshrink && upx -t /app/target/x86_64-unknown-linux-musl/release/linkshrink

FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/linkshrink /app/linkshrink
WORKDIR /app
COPY assets/* assets/
COPY templates/* templates/
EXPOSE 8080
CMD ["/app/linkshrink"]
