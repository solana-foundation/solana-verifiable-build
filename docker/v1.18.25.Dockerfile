FROM --platform=linux/amd64 rust@sha256:b7b25312e49dfbe6cab04c89d5a8ed5df2df971406a3b5c5ac43e247b5821b5f

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
RUN curl -sSfL "https://release.anza.xyz/v1.18.25/install" -o /tmp/solana_install.sh && \
    ACTUAL=$(sha256sum /tmp/solana_install.sh | awk '{print $1}') && \
    test "$ACTUAL" = "dfd7132b5c4b12054d20dd0e1d6488cbe65263b449429a1c8000ff76d88b5787" && \
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
