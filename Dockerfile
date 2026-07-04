FROM rust:1-bookworm AS builder

WORKDIR /build
COPY . .
RUN cargo build --release --bin recalld --bin recalld-cli

FROM debian:bookworm-slim

LABEL org.opencontainers.image.source="https://github.com/calebevans/recalld"
LABEL org.opencontainers.image.description="AI memory system with spaced-repetition decay"
LABEL org.opencontainers.image.licenses="AGPL-3.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/recalld /usr/local/bin/
COPY --from=builder /build/target/release/recalld-cli /usr/local/bin/

RUN mkdir -p /data

ENV RECALLD_BIND=0.0.0.0:7680
ENV RECALLD_STORAGE_DATA_DIR=/data

EXPOSE 7680
VOLUME /data

ENTRYPOINT ["recalld"]
CMD ["serve"]
