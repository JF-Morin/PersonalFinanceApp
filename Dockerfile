## Stage 1 — Build
FROM rust:1.82-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependency compilation
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null; rm -rf src

# Build real source
COPY src ./src
RUN touch src/main.rs && cargo build --release

## Stage 2 — Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/personal-finance-app .
COPY templates ./templates
COPY static    ./static

# Directories are mounted as volumes at runtime
RUN mkdir -p /app/data /app/storage/Statements /app/storage/Images /app/logs

EXPOSE 3000

ENV DATABASE_URL=sqlite:/app/data/finance.db
ENV STORAGE_PATH=/app/storage
ENV LOG_PATH=/app/logs
ENV SERVER_HOST=0.0.0.0
ENV SERVER_PORT=3000
ENV RUST_LOG=personal_finance_app=debug,tower_http=info

CMD ["./personal-finance-app"]
