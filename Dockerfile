# ============================================================
# Stage 1: Build the octos binary
# ============================================================
FROM rust:1.85-alpine AS builder

RUN apk add --no-cache musl-dev git pkgconfig

WORKDIR /src

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY crates/octos-core/Cargo.toml crates/octos-core/Cargo.toml
COPY crates/octos-llm/Cargo.toml crates/octos-llm/Cargo.toml
COPY crates/octos-memory/Cargo.toml crates/octos-memory/Cargo.toml
COPY crates/octos-agent/Cargo.toml crates/octos-agent/Cargo.toml
COPY crates/octos-bus/Cargo.toml crates/octos-bus/Cargo.toml
COPY crates/octos-cli/Cargo.toml crates/octos-cli/Cargo.toml

# Create stub lib.rs files for dependency caching
RUN mkdir -p crates/octos-core/src && echo "" > crates/octos-core/src/lib.rs && \
    mkdir -p crates/octos-llm/src && echo "" > crates/octos-llm/src/lib.rs && \
    mkdir -p crates/octos-memory/src && echo "" > crates/octos-memory/src/lib.rs && \
    mkdir -p crates/octos-agent/src && echo "" > crates/octos-agent/src/lib.rs && \
    mkdir -p crates/octos-bus/src && echo "" > crates/octos-bus/src/lib.rs && \
    mkdir -p crates/octos-cli/src && echo "fn main() {}" > crates/octos-cli/src/main.rs

RUN cargo build --release --bin octos 2>/dev/null || true

# Copy full source and build
COPY . .
RUN touch crates/*/src/*.rs && \
    cargo build --release --bin octos \
      --features telegram,discord,slack,whatsapp,feishu,email

# ============================================================
# Stage 2: Minimal runtime image
# ============================================================
FROM alpine:3.21

RUN apk add --no-cache ca-certificates tzdata \
    # Runtime deps for skills (pptx, mofa-pptx, browser)
    nodejs npm ffmpeg chromium \
    # LibreOffice + Poppler for office document conversion and visual QA
    libreoffice poppler-utils \
    # GCC for soffice sandbox shim (compiled on first use if needed)
    gcc musl-dev

# Install Node.js skill dependencies
RUN npm install -g pptxgenjs react-icons react react-dom sharp

# Copy binary
COPY --from=builder /src/target/release/octos /usr/local/bin/octos

# Copy builtin skills
COPY --from=builder /src/crates/octos-agent/skills /opt/octos/skills

# Create workspace
RUN mkdir -p /root/.octos/skills && \
    cp -r /opt/octos/skills/* /root/.octos/skills/ 2>/dev/null || true

ENTRYPOINT ["octos"]
CMD ["gateway"]
