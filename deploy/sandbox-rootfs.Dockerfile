ARG BASE_IMAGE=rust:1-slim-bookworm
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev ca-certificates curl git python3 \
    iproute2 procps file xz-utils && rm -rf /var/lib/apt/lists/* \
    && ln -s ../local/cargo/bin/cargo /usr/bin/cargo \
    && ln -s ../local/cargo/bin/rustc /usr/bin/rustc \
    && mkdir -p /work /opt/survive /proc /sys /dev/pts /dev/shm /run /tmp \
    && chmod 1777 /tmp \
    && printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d \
    && chmod 755 /usr/sbin/policy-rc.d
CMD ["/bin/true"]
