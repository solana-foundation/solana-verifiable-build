FROM --platform=linux/amd64 rust@sha256:f7c649bf7cc388826273a4977e8304778b7f8ad2e20838c1d52283cf23e49f7a

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
RUN curl -sSfL "https://release.anza.xyz/v4.0.3/install" -o /tmp/solana_install.sh && \
    ACTUAL=$(sha256sum /tmp/solana_install.sh | awk '{print $1}') && \
    test "$ACTUAL" = "9cefdb7493c9b42e91bf5d44663aea84042c5d2e6420a70aea10be11b7d74aaa" && \
    sh /tmp/solana_install.sh && \
    rm -f /tmp/solana_install.sh

ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
# Call cargo build-sbf to trigger installation of associated platform tools
RUN cargo init temp --edition 2021 && \
    cd temp && \
    cargo build-sbf && \
    rm -rf temp
WORKDIR /build

CMD /bin/bash
