# NanoClaw Agent Container
# Runs Claude Agent SDK in isolated Linux VM with browser automation

FROM node:22-slim

# Install system dependencies for Chromium
RUN apt-get update && apt-get install -y \
    chromium \
    fonts-liberation \
    fonts-noto-color-emoji \
    libgbm1 \
    libnss3 \
    libatk-bridge2.0-0 \
    libgtk-3-0 \
    libx11-xcb1 \
    libxcomposite1 \
    libxdamage1 \
    libxrandr2 \
    libasound2 \
    libpangocairo-1.0-0 \
    libcups2 \
    libdrm2 \
    libxshmfence1 \
    curl \
    git \
    && rm -rf /var/lib/apt/lists/*

# Set Chromium path for agent-browser
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/bin/chromium
ENV PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH=/usr/bin/chromium

# Install agent-browser and claude-code globally
RUN npm install -g agent-browser @anthropic-ai/claude-code

# Create app directory with same structure as source:
#   /app/agent-runner/src/   (runner code)
#   /app/shared/             (shared code)
# This preserves the ../../shared/ import paths
WORKDIR /app/agent-runner

# Copy package files first for better caching
COPY agent-runner/package*.json ./

# Install dependencies
RUN npm install

# Copy shared code
COPY shared/ /app/shared/

# Copy source code
COPY agent-runner/ ./

# Build TypeScript (rootDir is .. which is /app)
RUN npx tsc

# Create workspace directories
RUN mkdir -p /workspace/group /workspace/global /workspace/extra /workspace/ipc/messages /workspace/ipc/tasks /workspace/ipc/input

# Create entrypoint script
# Recompiles TypeScript on startup (allows live code changes via host-mounted source)
# Host mounts: runner src → /app/agent-runner/src, shared → /app/shared
# Output goes to /tmp/dist/agent-runner/src/index.js (matching rootDir: "..")
# Secrets are passed via stdin JSON — temp file is deleted immediately after Node reads it
RUN printf '#!/bin/bash\nset -e\ncd /app/agent-runner && npx tsc --outDir /tmp/dist 2>&1 >&2\nln -s /app/agent-runner/node_modules /tmp/dist/agent-runner/node_modules 2>/dev/null || true\nchmod -R a-w /tmp/dist\ncat > /tmp/input.json\nnode /tmp/dist/agent-runner/src/index.js < /tmp/input.json\n' > /app/entrypoint.sh && chmod +x /app/entrypoint.sh

# Set ownership to node user (non-root) for writable directories
RUN chown -R node:node /workspace && chmod 777 /home/node

# Switch to non-root user (required for --dangerously-skip-permissions)
USER node

# Set working directory to group workspace
WORKDIR /workspace/group

# Entry point reads JSON from stdin, outputs JSON to stdout
ENTRYPOINT ["/app/entrypoint.sh"]
