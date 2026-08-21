FROM rust:1.95-alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /eris
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/eris/target \
  cargo build --release --locked -p eris-server -p eris-migrate && \
  cp target/release/eris-server target/release/eris-migrate /usr/local/bin/

FROM alpine:3.22

COPY --from=build /usr/local/bin/eris-server /usr/local/bin/eris-migrate /usr/local/bin/

USER nobody
EXPOSE 5588
CMD ["eris-server"]
