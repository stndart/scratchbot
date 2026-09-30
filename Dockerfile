FROM rust:1.97-bullseye
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ARG GIT_SHA=dev
ENV GIT_SHA=${GIT_SHA}
RUN cargo test --release \
    && cargo build --release \
    && mkdir -p /artifacts \
    && cp target/release/scratchwall-telegram /artifacts/scratchwall-telegram
