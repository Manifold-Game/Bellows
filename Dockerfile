FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p bellows-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 bellows \
    && install -d -o bellows -g bellows /var/lib/bellows
COPY --from=build /src/target/release/bellowsd /usr/local/bin/bellowsd
USER bellows
VOLUME ["/var/lib/bellows"]
EXPOSE 7878
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:7878/live >/dev/null || exit 1
ENTRYPOINT ["bellowsd", "--listen", "0.0.0.0:7878", "--data-dir", "/var/lib/bellows"]
