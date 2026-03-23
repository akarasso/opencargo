# Stage 1: Frontend
FROM node:22-alpine AS frontend
RUN npm install -g pnpm@10
WORKDIR /app/frontend
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY frontend/ .
RUN pnpm build

# Stage 2: Rust build
FROM rust:1.88-alpine AS builder
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static perl
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src frontend/dist && echo "fn main() {}" > src/main.rs && cargo build --release 2>/dev/null || true && rm -rf src
COPY src ./src
COPY --from=frontend /app/frontend/dist ./frontend/dist
RUN touch src/main.rs && cargo build --release

# Stage 3: Runtime
FROM alpine:3.21
COPY --from=builder /app/target/release/opencargo /usr/local/bin/
EXPOSE 6789
ENTRYPOINT ["opencargo"]
