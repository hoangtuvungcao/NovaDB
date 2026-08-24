FROM rust:1.93-bookworm AS builder
WORKDIR /source
COPY . .
RUN cargo build --locked --release -p novadb-server --bin novadbd

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /var/lib/novadb novadb \
    && mkdir -p /var/lib/novadb/databases \
    && chown -R novadb:novadb /var/lib/novadb
COPY --from=builder /source/target/release/novadbd /usr/local/bin/novadbd
USER novadb
WORKDIR /var/lib/novadb
VOLUME ["/var/lib/novadb"]
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/novadbd"]
CMD ["--listen", "0.0.0.0:8787", "--database-path", "/var/lib/novadb/relay.sqlite3", "--data-dir", "/var/lib/novadb/databases"]

