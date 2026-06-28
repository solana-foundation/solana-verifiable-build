FROM --platform=linux/amd64 rust@sha256:b7b25312e49dfbe6cab04c89d5a8ed5df2df971406a3b5c5ac43e247b5821b5f

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
RUN curl -sSfL "https://release.anza.xyz/v1.18.26/install" -o /tmp/solana_install.sh && \
    ACTUAL=$(sha256sum /tmp/solana_install.sh | awk '{print $1}') && \
    test "$ACTUAL" = "cec72cde1cf36eb35cd8326245d23af0b6791fab68337c2953e2ca2a40af2c50" && \
    sh /tmp/solana_install.sh && \
    rm -f /tmp/solana_install.sh

ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
# Call cargo test-sbf to trigger installation of associated platform tools
RUN cargo init --lib temp --edition 2021 && \
    cd temp && \
    echo "[lib]" >> Cargo.toml && \
    echo 'crate-type = ["cdylib", "lib"]' >> Cargo.toml && \
    cargo test-sbf && \
    cd ../ && \
    rm -rf temp
WORKDIR /build

CMD /bin/bash
