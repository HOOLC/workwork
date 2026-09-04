FROM rust:1-alpine AS build

RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --locked --release -p zork-agent

FROM scratch AS export
COPY --from=build /src/target/release/zork-agent /zork-agent
