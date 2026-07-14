FROM --platform=linux/amd64 rust@sha256:443dd9a3260cf23c22fc05051dd5661dd7b4028d3d25dbaffab6563b63c3539c

LABEL agave.version="v4.1.2"
RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
# cargo-build-sbf 4.1.0 is the crate pin for the Agave 4.1.x train
RUN cargo install cargo-build-sbf --version 4.1.0 --locked
# Call cargo build-sbf to trigger installation of platform tools
RUN cargo init temp --edition 2021 && \
    cd temp && \
    cargo build-sbf --tools-version v1.54 && \
    rm -rf temp
WORKDIR /build

CMD /bin/bash
