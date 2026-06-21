#!/usr/bin/env bash
# Deploy dragonfly-tracker to a t4g.nano EC2 instance.
#
# Prerequisites:
#   - AWS credentials configured: aws configure
#   - Tracker image pushed to ghcr.io: ./deploy/ec2/build-tracker.sh
#
# Usage:
#   ./deploy/ec2/deploy.sh [github-username]
set -euo pipefail

GITHUB_USER="${1:-JustDory}"
REGION="${AWS_DEFAULT_REGION:-us-east-1}"
INSTANCE_TYPE="t4g.nano"
IMAGE="ghcr.io/${GITHUB_USER,,}/dragonfly-gguf-client/tracker:latest"
SG_NAME="dragonfly-gguf-tracker-sg"
KEY_NAME="dragonfly-gguf-key"
INSTANCE_NAME="dragonfly-gguf-tracker"

echo "==> dragonfly-gguf tracker EC2 deployment"
echo "    Region:  $REGION"
echo "    Type:    $INSTANCE_TYPE"
echo "    Image:   $IMAGE"
echo ""

# Verify credentials
aws sts get-caller-identity --query 'Account' --output text > /dev/null \
  || { echo "ERROR: No AWS credentials. Run: aws configure"; exit 1; }

# Latest Ubuntu 24.04 LTS ARM64 AMI (Canonical account 099720109477)
echo "Fetching latest Ubuntu 24.04 ARM64 AMI..."
AMI_ID=$(aws ec2 describe-images \
  --region "$REGION" \
  --owners 099720109477 \
  --filters \
    "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-arm64-server-*" \
    "Name=state,Values=available" \
  --query 'sort_by(Images, &CreationDate)[-1].ImageId' \
  --output text)
echo "    AMI: $AMI_ID"

# Security group (idempotent)
echo "Creating security group..."
SG_ID=$(aws ec2 create-security-group \
    --region "$REGION" \
    --group-name "$SG_NAME" \
    --description "dragonfly-gguf tracker: TCP 8080 + SSH 22" \
    --query 'GroupId' --output text 2>/dev/null) \
  || SG_ID=$(aws ec2 describe-security-groups \
      --region "$REGION" \
      --filters "Name=group-name,Values=$SG_NAME" \
      --query 'SecurityGroups[0].GroupId' --output text)

aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG_ID" \
  --protocol tcp --port 22 --cidr 0.0.0.0/0 2>/dev/null || true
aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG_ID" \
  --protocol tcp --port 8080 --cidr 0.0.0.0/0 2>/dev/null || true
echo "    Security group: $SG_ID"

# SSH key pair (created once, saved locally)
if ! aws ec2 describe-key-pairs --region "$REGION" --key-names "$KEY_NAME" &>/dev/null; then
  echo "Creating SSH key pair..."
  aws ec2 create-key-pair \
    --region "$REGION" \
    --key-name "$KEY_NAME" \
    --query 'KeyMaterial' \
    --output text > "${KEY_NAME}.pem"
  chmod 600 "${KEY_NAME}.pem"
  echo "    Key saved to ${KEY_NAME}.pem — back this up!"
else
  echo "    Key pair $KEY_NAME already exists."
fi

# User data: install Docker, pull image, run tracker
USER_DATA=$(cat <<USERDATA
#!/bin/bash
set -euo pipefail
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io
systemctl enable --now docker
docker run -d \
  --name gguf-tracker \
  --restart unless-stopped \
  -p 8080:8080 \
  -e TRACKER_BIND=0.0.0.0:8080 \
  -e TRACKER_TTL=1800 \
  -e TRACKER_RATE_LIMIT=10 \
  ${IMAGE}
USERDATA
)

# Launch
echo "Launching $INSTANCE_TYPE..."
INSTANCE_ID=$(aws ec2 run-instances \
  --region "$REGION" \
  --image-id "$AMI_ID" \
  --instance-type "$INSTANCE_TYPE" \
  --key-name "$KEY_NAME" \
  --security-group-ids "$SG_ID" \
  --user-data "$USER_DATA" \
  --tag-specifications \
    "ResourceType=instance,Tags=[{Key=Name,Value=$INSTANCE_NAME},{Key=Project,Value=dragonfly-gguf}]" \
  --query 'Instances[0].InstanceId' \
  --output text)

echo "    Instance: $INSTANCE_ID"
echo "    Waiting for running state..."
aws ec2 wait instance-running --region "$REGION" --instance-ids "$INSTANCE_ID"

PUBLIC_IP=$(aws ec2 describe-instances \
  --region "$REGION" \
  --instance-ids "$INSTANCE_ID" \
  --query 'Reservations[0].Instances[0].PublicIpAddress' \
  --output text)

echo ""
echo "============================================================"
echo "  Tracker deployed!"
echo "============================================================"
echo "  Instance:  $INSTANCE_ID"
echo "  Public IP: $PUBLIC_IP"
echo ""
echo "  Next steps:"
echo "  1. Point tracker.dragonfly-gguf.dev A record -> $PUBLIC_IP"
echo "  2. Wait ~60s for Docker startup, then verify:"
echo "     curl http://$PUBLIC_IP:8080/peers?content_key=0000000000000000000000000000000000000000000000000000000000000000"
echo ""
echo "  SSH access:"
echo "     ssh -i ${KEY_NAME}.pem ubuntu@$PUBLIC_IP"
echo "============================================================"
