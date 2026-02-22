#!/bin/bash
# Build NanoClaw agent container images (all runtimes)
#
# Usage:
#   ./build.sh              # Build all images with :latest tag
#   ./build.sh v1.2.3       # Build all images with specific tag
#   ./build.sh latest claude # Build only Claude image
#   ./build.sh latest gemini # Build only Gemini image
#   ./build.sh latest codex  # Build only Codex image

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

TAG="${1:-latest}"
RUNTIME="${2:-all}"
CONTAINER_RUNTIME="${CONTAINER_RUNTIME:-docker}"

build_image() {
  local name="$1"
  local dockerfile="$2"
  echo ""
  echo "=== Building ${name}:${TAG} ==="
  ${CONTAINER_RUNTIME} build -f "$dockerfile" -t "${name}:${TAG}" .
  echo "Built: ${name}:${TAG}"
}

if [ "$RUNTIME" = "all" ] || [ "$RUNTIME" = "claude" ]; then
  build_image "nanoclaw-agent" "Dockerfile"
fi

if [ "$RUNTIME" = "all" ] || [ "$RUNTIME" = "gemini" ]; then
  build_image "nanoclaw-agent-gemini" "Dockerfile.gemini"
fi

if [ "$RUNTIME" = "all" ] || [ "$RUNTIME" = "codex" ]; then
  build_image "nanoclaw-agent-codex" "Dockerfile.codex"
fi

echo ""
echo "Build complete!"
echo ""
echo "Test with:"
echo "  echo '{\"prompt\":\"What is 2+2?\",\"groupFolder\":\"test\",\"chatJid\":\"test@g.us\",\"isMain\":false}' | ${CONTAINER_RUNTIME} run -i nanoclaw-agent:${TAG}"
echo "  echo '{\"prompt\":\"What is 2+2?\",\"groupFolder\":\"test\",\"chatJid\":\"test@g.us\",\"isMain\":false,\"secrets\":{\"GEMINI_REFRESH_TOKEN\":\"...\",\"GEMINI_OAUTH_CLIENT_ID\":\"...\",\"GEMINI_OAUTH_CLIENT_SECRET\":\"...\"}}' | ${CONTAINER_RUNTIME} run -i nanoclaw-agent-gemini:${TAG}"
