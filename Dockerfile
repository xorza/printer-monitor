FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --uid 1000 --no-create-home appuser
COPY --from=builder /app/target/release/printer-monitor /usr/local/bin/
USER appuser
CMD ["printer-monitor"]
