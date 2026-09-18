# Stage 1: Frontend
FROM node:22-alpine AS frontend
RUN npm install -g pnpm@10
WORKDIR /app/frontend
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY frontend/ .
RUN pnpm build

# Stage 2: Rust build
FROM rust:1.93-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# The manifest declares a workspace, so every member's manifest must be present
# before cargo can even resolve: the priming build stubs the domain crate the
# same way it stubs the binary.
COPY crates/domain/Cargo.toml crates/domain/
RUN mkdir -p src crates/domain/src frontend/dist \
  && echo "fn main() {}" > src/main.rs && : > crates/domain/src/lib.rs \
  && cargo build --release 2>/dev/null || true \
  && rm -rf src crates
COPY src ./src
COPY crates ./crates
COPY --from=frontend /app/frontend/dist ./frontend/dist
RUN touch src/main.rs crates/domain/src/lib.rs && cargo build --release --locked

# Stage 3: Runtime
FROM alpine:3.21
# Run as an unprivileged user (uid/gid 10001). Matches runAsUser/runAsGroup in
# the k8s/Helm securityContext. Writable state lives under /data (a volume with
# matching fsGroup), so the binary never needs to write to the image rootfs.
RUN addgroup -S -g 10001 opencargo && adduser -S -u 10001 -G opencargo opencargo
COPY --from=builder /app/target/release/opencargo /usr/local/bin/
RUN mkdir -p /data && chown 10001:10001 /data
VOLUME ["/data"]
WORKDIR /
USER 10001:10001
EXPOSE 6789
ENTRYPOINT ["opencargo"]
CMD ["--bind", "0.0.0.0:6789"]
