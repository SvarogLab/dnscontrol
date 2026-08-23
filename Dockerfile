# Packaging only - expects the binary already cross-compiled locally/in CI via:
#   cargo zigbuild --release --target x86_64-unknown-linux-musl
# (a full rustc build segfaults under QEMU emulation for a non-native --platform, so nothing is
# compiled inside this Dockerfile.)
#
# google-cloud-auth's reqwest/rustls stack verifies googleapis.com through rustls-platform-verifier,
# which reads the OS trust store from the filesystem - there is no compiled-in root bundle, so a
# bare `FROM scratch` makes every API call fail TLS verification. Pull just that one file from
# Alpine's ca-certificates package, not the rest of the distro.
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ARG TARGETARCH
COPY target/${TARGETARCH}/release/dnscontrol /dnscontrol
ENTRYPOINT ["/dnscontrol"]
