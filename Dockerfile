# ============================================================
# Stage 1: Build the octos binary
# ============================================================
FROM rust:1.88-alpine AS builder

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
COPY crates/octos-pipeline/Cargo.toml crates/octos-pipeline/Cargo.toml
COPY crates/octos-plugin/Cargo.toml crates/octos-plugin/Cargo.toml
COPY crates/app-skills/news/Cargo.toml crates/app-skills/news/Cargo.toml
COPY crates/app-skills/deep-search/Cargo.toml crates/app-skills/deep-search/Cargo.toml
COPY crates/app-skills/deep-crawl/Cargo.toml crates/app-skills/deep-crawl/Cargo.toml
COPY crates/app-skills/send-email/Cargo.toml crates/app-skills/send-email/Cargo.toml
COPY crates/app-skills/account-manager/Cargo.toml crates/app-skills/account-manager/Cargo.toml
COPY crates/app-skills/time/Cargo.toml crates/app-skills/time/Cargo.toml
COPY crates/app-skills/weather/Cargo.toml crates/app-skills/weather/Cargo.toml
COPY crates/platform-skills/voice/Cargo.toml crates/platform-skills/voice/Cargo.toml
COPY crates/octos-diagnostics/Cargo.toml crates/octos-diagnostics/Cargo.toml
COPY crates/octos-dora-mcp/Cargo.toml crates/octos-dora-mcp/Cargo.toml
COPY crates/octos-sandbox/Cargo.toml crates/octos-sandbox/Cargo.toml
COPY crates/octos-swarm/Cargo.toml crates/octos-swarm/Cargo.toml
COPY crates/app-skills/wechat-bridge/Cargo.toml crates/app-skills/wechat-bridge/Cargo.toml
COPY crates/app-skills/skill-evolve/Cargo.toml crates/app-skills/skill-evolve/Cargo.toml
COPY crates/app-skills/harness-starter-generic/Cargo.toml crates/app-skills/harness-starter-generic/Cargo.toml
COPY crates/app-skills/harness-starter-report/Cargo.toml crates/app-skills/harness-starter-report/Cargo.toml
COPY crates/app-skills/harness-starter-audio/Cargo.toml crates/app-skills/harness-starter-audio/Cargo.toml
COPY crates/app-skills/harness-starter-coding/Cargo.toml crates/app-skills/harness-starter-coding/Cargo.toml

# Create stub source files for dependency caching
# Library crates get lib.rs, binary crates get main.rs
RUN mkdir -p crates/octos-core/src && echo "" > crates/octos-core/src/lib.rs && \
    mkdir -p crates/octos-llm/src && echo "" > crates/octos-llm/src/lib.rs && \
    mkdir -p crates/octos-memory/src && echo "" > crates/octos-memory/src/lib.rs && \
    mkdir -p crates/octos-agent/src && echo "" > crates/octos-agent/src/lib.rs && \
    mkdir -p crates/octos-bus/src && echo "" > crates/octos-bus/src/lib.rs && \
    mkdir -p crates/octos-cli/src && echo "fn main() {}" > crates/octos-cli/src/main.rs && \
    mkdir -p crates/octos-pipeline/src && echo "" > crates/octos-pipeline/src/lib.rs && \
    mkdir -p crates/octos-plugin/src && echo "" > crates/octos-plugin/src/lib.rs && \
    mkdir -p crates/app-skills/news/src && echo "fn main() {}" > crates/app-skills/news/src/main.rs && \
    mkdir -p crates/app-skills/deep-search/src && echo "fn main() {}" > crates/app-skills/deep-search/src/main.rs && \
    mkdir -p crates/app-skills/deep-crawl/src && echo "fn main() {}" > crates/app-skills/deep-crawl/src/main.rs && \
    mkdir -p crates/app-skills/send-email/src && echo "fn main() {}" > crates/app-skills/send-email/src/main.rs && \
    mkdir -p crates/app-skills/account-manager/src && echo "fn main() {}" > crates/app-skills/account-manager/src/main.rs && \
    mkdir -p crates/app-skills/time/src && echo "fn main() {}" > crates/app-skills/time/src/main.rs && \
    mkdir -p crates/app-skills/weather/src && echo "fn main() {}" > crates/app-skills/weather/src/main.rs && \
    mkdir -p crates/platform-skills/voice/src && echo "fn main() {}" > crates/platform-skills/voice/src/main.rs && \
    mkdir -p crates/octos-diagnostics/src && echo "" > crates/octos-diagnostics/src/lib.rs && \
    mkdir -p crates/octos-dora-mcp/src && echo "" > crates/octos-dora-mcp/src/lib.rs && \
    mkdir -p crates/octos-sandbox/src && echo "fn main() {}" > crates/octos-sandbox/src/main.rs && \
    mkdir -p crates/octos-swarm/src && echo "" > crates/octos-swarm/src/lib.rs && \
    mkdir -p crates/app-skills/wechat-bridge/src && echo "fn main() {}" > crates/app-skills/wechat-bridge/src/main.rs && \
    mkdir -p crates/app-skills/skill-evolve/src && echo "fn main() {}" > crates/app-skills/skill-evolve/src/main.rs && \
    mkdir -p crates/app-skills/harness-starter-generic/src && echo "" > crates/app-skills/harness-starter-generic/src/lib.rs && echo "fn main() {}" > crates/app-skills/harness-starter-generic/src/main.rs && \
    mkdir -p crates/app-skills/harness-starter-report/src && echo "" > crates/app-skills/harness-starter-report/src/lib.rs && echo "fn main() {}" > crates/app-skills/harness-starter-report/src/main.rs && \
    mkdir -p crates/app-skills/harness-starter-audio/src && echo "" > crates/app-skills/harness-starter-audio/src/lib.rs && echo "fn main() {}" > crates/app-skills/harness-starter-audio/src/main.rs && \
    mkdir -p crates/app-skills/harness-starter-coding/src && echo "" > crates/app-skills/harness-starter-coding/src/lib.rs && echo "fn main() {}" > crates/app-skills/harness-starter-coding/src/main.rs

RUN cargo build --release --bin octos \
      -p octos-cli \
      --features api,telegram,discord,slack,whatsapp,feishu,email,audio_mp3 \
      2>/dev/null || true

# Copy full source and build
COPY . .
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release --bin octos \
      -p octos-cli \
      --features api,telegram,discord,slack,whatsapp,feishu,email,matrix,audio_mp3

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
