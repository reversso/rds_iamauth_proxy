ARG RUST_VERSION=1.96.0-stable-2026-06-15
ARG CARGO_CHEF_VERSION=0.1.77

FROM clux/muslrust:${RUST_VERSION} AS chef
USER root
ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --version "${CARGO_CHEF_VERSION}" --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --target "$(arch)-unknown-linux-musl" --recipe-path recipe.json
COPY . .
RUN cargo build --release --target "$(arch)-unknown-linux-musl" --bin rds_proxy && \
    mv "target/$(arch)-unknown-linux-musl/release/rds_proxy" /rds_proxy

FROM alpine:3.24 AS runtime
RUN apk add --no-cache ca-certificates && \
    addgroup -S rdsproxy && \
    adduser -S rdsproxy -G rdsproxy
COPY --from=builder /rds_proxy /usr/local/bin/
USER rdsproxy
CMD ["/usr/local/bin/rds_proxy", "--listen", "0.0.0.0:5435"]
