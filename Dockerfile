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
# Run as an unprivileged user (uid/gid 10001). Matches runAsUser/runAsGroup in
# the k8s/Helm securityContext. Writable state lives under /data (a volume with
# matching fsGroup), so the binary never needs to write to the image rootfs.
RUN addgroup -S -g 10001 opencargo && adduser -S -u 10001 -G opencargo opencargo
COPY --from=builder /app/target/release/opencargo /usr/local/bin/
USER 10001:10001
EXPOSE 6789
ENTRYPOINT ["opencargo"]
