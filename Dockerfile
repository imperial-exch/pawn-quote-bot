# Build:  docker build -t pawn-quote-bot .
# Run:    docker run --rm \
#           -v "$PWD/config.toml:/etc/pawn/config.toml:ro" \
#           -v "$PWD/quote-key.json:/etc/pawn/quote-key.json:ro" \
#           pawn-quote-bot
#
# The config and the quoting key are MOUNTED, never baked into the image. An
# image layer is world-readable to anyone who can pull it, and the key in that
# file is the one that signs your appraisals.

# Pinned rather than `latest`: this crate is built and tested against 1.86 in
# the repository it is mirrored from, and a silent toolchain bump is not
# something a market maker should discover from a failed build at 3am.
FROM rust:1.86-slim AS builder
WORKDIR /build

# reqwest links the system OpenSSL, so the builder needs the headers and the
# runtime stage below needs the library.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .
# --locked: build the dependency versions this repo was tested with, not
# whatever has been published since.
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 maker

COPY --from=builder /build/target/release/pawn-quote-bot /usr/local/bin/pawn-quote-bot

# Non-root: the container reads a signing key and talks to the internet, and it
# never needs to write anything.
USER maker
ENV QUOTE_BOT_CONFIG=/etc/pawn/config.toml

ENTRYPOINT ["pawn-quote-bot"]
