FROM --platform=linux/amd64 rust@sha256:479476fa1dec14dfa9ed2dbcaa94cda5ab945e125d45c2d153267cc0135f3b69

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
RUN curl -sSfL "https://release.anza.xyz/v2.2.12/install" -o /tmp/solana_install.sh && \
    ACTUAL=$(sha256sum /tmp/solana_install.sh | awk '{print $1}') && \
    test "$ACTUAL" = "1430da752981d0b3b09cb35d73adede97ec82f04a8e2d2fb0780d547b01c555a" && \
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
