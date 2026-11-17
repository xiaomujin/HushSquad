# HushSquad 服务器镜像。
#
#   docker build -f deploy/Dockerfile -t hushsquad-server .
#
# 多阶段：builder 里编译，运行镜像只留一个静态二进制 + 证书/用户表挂载点。

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
# 先只拷清单，把依赖层缓存起来；改代码不会重编依赖。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
COPY docs/proto ./docs/proto
RUN cargo build --release -p hushsquad-server

FROM debian:bookworm-slim
RUN useradd --system --home /var/lib/hushsquad --shell /usr/sbin/nologin hushsquad
COPY --from=builder /src/target/release/hushsquad-server /usr/local/bin/hushsquad-server
RUN mkdir -p /etc/hushsquad /var/lib/hushsquad && chown hushsquad:hushsquad /var/lib/hushsquad
USER hushsquad
WORKDIR /var/lib/hushsquad
EXPOSE 64738/tcp 64739/udp
ENTRYPOINT ["/usr/local/bin/hushsquad-server"]
CMD ["--db", "/var/lib/hushsquad/hushsquad.db"]
