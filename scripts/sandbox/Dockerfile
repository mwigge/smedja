FROM alpine:3.20

RUN apk add --no-cache \
    bash \
    curl \
    git \
    jq \
    ripgrep \
    fd \
    && rm -rf /var/cache/apk/*

WORKDIR /workspace
