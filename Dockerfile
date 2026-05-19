# Multi-stage Dockerfile for building a static musl-linked binary and
# running it on a minimal Alpine image.

####################################
# Builder stage (musl target)
####################################
FROM clux/muslrust:stable as builder

WORKDIR /usr/src/fz_bot

# Copy source
COPY . .

# Build release binary for musl
RUN cargo build --release --target x86_64-unknown-linux-musl

####################################
# Runtime stage (Alpine)
####################################
FROM alpine:3.18

RUN apk add --no-cache ca-certificates

# Copy the musl-built binary from the builder stage
COPY --from=builder /usr/src/fz_bot/target/x86_64-unknown-linux-musl/release/fz_bot /usr/local/bin/fz_bot

RUN adduser -D -g '' fzuser && chown fzuser:fzuser /usr/local/bin/fz_bot

USER fzuser

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/fz_bot"]
