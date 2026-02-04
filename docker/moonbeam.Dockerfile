# Moonbeam Binary
#
# Requires to run from repository root and to copy the binary in the build folder (part of the release workflow)
FROM rust:1.88-trixie AS builder


WORKDIR /moonbeam/

RUN echo "*** Installing Basic dependencies ***"
RUN apt-get update && apt-get install -y ca-certificates && update-ca-certificates
RUN apt install --assume-yes git clang curl openssl libssl-dev llvm libudev-dev make protobuf-compiler pkg-config

COPY . .

RUN cargo build --release -j 8

FROM docker.io/library/ubuntu:24.04

RUN apt-get update && apt-get install -y ca-certificates && update-ca-certificates

COPY --from=builder /moonbeam/target/release/moonbeam /moonbeam/moonbeam

RUN chmod 777 /moonbeam/moonbeam*

# 30333 for parachain p2p
# 30334 for relaychain p2p
# 9933 for RPC call
# 9944 for Websocket
# 9615 for Prometheus (metrics)
EXPOSE 30333 30334 9933 9944 9615

VOLUME ["/data"]

ENTRYPOINT ["/moonbeam/moonbeam"]