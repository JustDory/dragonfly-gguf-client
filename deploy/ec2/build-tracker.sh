#!/usr/bin/env bash
# Build dragonfly-tracker for linux/arm64 and push to ghcr.io.
#
# Prerequisites:
#   1. Docker with buildx + QEMU (one-time setup):
#        docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
#        docker buildx create --use --name multiarch
#   2. GitHub personal access token with write:packages scope:
#        echo $GITHUB_TOKEN | docker login ghcr.io -u YOUR_GITHUB_USERNAME --password-stdin
#
# Run from the repo root:
#   ./deploy/ec2/build-tracker.sh [github-username]
set -euo pipefail

GITHUB_USER="${1:-JustDory}"
IMAGE="ghcr.io/${GITHUB_USER,,}/dragonfly-gguf-client/tracker:latest"

echo "==> Building dragonfly-tracker for linux/arm64"
echo "    Image: $IMAGE"
echo ""

# Verify buildx is set up
if ! docker buildx inspect multiarch &>/dev/null; then
  echo "Setting up QEMU + buildx..."
  docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
  docker buildx create --use --name multiarch
fi

# Build and push (context must be repo root because Dockerfile COPYs workspace crates)
docker buildx build \
  --platform linux/arm64 \
  --file deploy/tracker/Dockerfile \
  --tag "$IMAGE" \
  --push \
  .

echo ""
echo "Pushed: $IMAGE"
echo "EC2 instances can now pull this image without auth (public repo)."
