# Packaging only: the binary is the musl release build for TARGETARCH, staged at
# target/<arch>/release/ by CI or by hand.
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
