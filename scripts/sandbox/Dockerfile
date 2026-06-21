FROM alpine:3.20

RUN apk add --no-cache \
    bash \
    git \
    jq \
    ripgrep \
    fd

RUN adduser -D -H -h /workspace smedja
USER smedja

WORKDIR /workspace
