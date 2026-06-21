#!/usr/bin/env bash
# Deploy nginx landing page + reverse-proxy to the tracker on port 8080.
# Run on the EC2 instance: sudo bash setup-nginx.sh
set -euo pipefail

REPO_URL="https://raw.githubusercontent.com/JustDory/dragonfly-gguf-client/main"
WEB_ROOT="/var/www/dragonfly-gguf"

echo "==> Installing nginx..."
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y nginx

echo "==> Creating web root..."
mkdir -p "$WEB_ROOT"

# Fetch the landing page from the repo (or copy from local if running in checkout)
if [[ -f "$(dirname "$0")/index.html" ]]; then
  cp "$(dirname "$0")/index.html" "$WEB_ROOT/index.html"
else
  curl -sSf "$REPO_URL/deploy/ec2/index.html" -o "$WEB_ROOT/index.html"
fi

echo "==> Writing nginx config..."
cat > /etc/nginx/sites-available/dragonfly-gguf <<'NGINX'
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;

    root /var/www/dragonfly-gguf;
    index index.html;

    # Landing page
    location = / {
        try_files /index.html =404;
    }

    # Proxy tracker API endpoints to the Rust binary on 8080
    location ~ ^/(peers|announce|leave)$ {
        proxy_pass         http://127.0.0.1:8080;
        proxy_set_header   Host $host;
        proxy_set_header   X-Real-IP $remote_addr;
        proxy_read_timeout 10s;
    }

    # Block everything else
    location / {
        return 404;
    }
}
NGINX

ln -sf /etc/nginx/sites-available/dragonfly-gguf /etc/nginx/sites-enabled/dragonfly-gguf
rm -f /etc/nginx/sites-enabled/default

echo "==> Testing nginx config..."
nginx -t

echo "==> Reloading nginx..."
systemctl enable --now nginx
systemctl reload nginx

echo ""
echo "Done. Landing page: http://$(curl -s ifconfig.me)/"
echo "Tracker API:        http://$(curl -s ifconfig.me)/peers?content_key=0000000000000000000000000000000000000000000000000000000000000000"
