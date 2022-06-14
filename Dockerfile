FROM rust:alpine AS builder

COPY . /build
WORKDIR /build
RUN apk update && \
    apk add musl-dev && \
    cargo build --release

FROM scratch
COPY --from=builder /build/target/release/pandoc-bot /
CMD [ "/pandoc-bot" ]
