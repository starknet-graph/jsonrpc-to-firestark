FROM rust:slim-bullseye AS build

WORKDIR /src
COPY . /src

RUN cargo build --release

FROM debian:bullseye-slim

LABEL org.opencontainers.image.source=https://github.com/starknet-graph/jsonrpc-to-firestark

COPY --from=build /src/target/release/jsonrpc-to-firestark /usr/bin/

ENTRYPOINT [ "jsonrpc-to-firestark" ]
