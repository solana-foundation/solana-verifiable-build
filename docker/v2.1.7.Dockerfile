FROM --platform=linux/amd64 rust@sha256:62afc139057dc9d3eda02e490677911b55a208ba22d6f7315f3c5c5851e31a36

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
RUN curl -sSfL "https://release.anza.xyz/v2.1.7/install" -o /tmp/solana_install.sh && \
    ACTUAL=$(sha256sum /tmp/solana_install.sh | awk '{print $1}') && \
    test "$ACTUAL" = "12f1d8ac0e51dccbf020c907cfadf2c7f70f78cdc42557a5c136d809591b85b3" && \
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
