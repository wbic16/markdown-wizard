#!/usr/bin/env bash
# Install md.phext.io on awsbox in one run. See deploy/DEPLOY.md for
# what this does and why, step by step, before running it.
#
# Run it as a sudo-capable login on the box (sudo on awsbox is a
# human's keystroke by house rule — this script never asks for a
# password itself; it uses sudo only for the system-level steps).
#
#   git clone https://github.com/wbic16/markdown-wizard.git /source/markdown-wizard
#   /source/markdown-wizard/deploy/install.sh
#
# Idempotent in shape: re-running rebuilds, reinstalls the unit and
# the site, restarts the service, and skips certbot when the
# certificate already exists.
set -euo pipefail

SRC=/source/markdown-wizard
DATA=/source/markdown-wizard-data
DOMAIN=md.phext.io
PORT=8090

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

echo "== markdown-wizard install =="

# 1. Source: clone if absent, otherwise take the checkout as it is.
if [ ! -d "$SRC/.git" ]; then
  echo "-- cloning $SRC"
  $SUDO mkdir -p /source
  git clone https://github.com/wbic16/markdown-wizard.git "$SRC"
else
  echo "-- using existing checkout at $SRC (git pull is yours to run)"
fi

# 2. Build on the box, the phext-edit way.
command -v cargo > /dev/null || { echo "cargo not found on this host; install Rust first" >&2; exit 1; }
echo "-- cargo build --release"
(cd "$SRC" && cargo build --release)

# 2b. PDF output is printed by a headless Chromium (see DEPLOY.md).
#     Absent one, the service still runs whole — bundles simply
#     have no PDFs, and each .pdf address says so. Warn, never fail.
CHROME_FOUND=""
for c in chromium chromium-browser google-chrome google-chrome-stable chrome-headless-shell; do
  if command -v "$c" > /dev/null; then CHROME_FOUND="$c"; break; fi
done
if [ -n "$CHROME_FOUND" ]; then
  echo "-- chromium for PDF output: $CHROME_FOUND"
else
  echo "!! no Chromium found — PDF output will be unavailable until one is installed" >&2
  echo "!! (e.g. apt install chromium); everything else works without it" >&2
fi

# 3. Data directory: bundles live here, one directory per slug.
echo "-- data dir $DATA"
$SUDO mkdir -p "$DATA"
$SUDO chown "$(id -un)":"$(id -gn)" "$DATA" || true

# 4. The service.
echo "-- systemd unit"
$SUDO install -m 0644 "$SRC/deploy/markdown-wizard.service" /etc/systemd/system/markdown-wizard.service
$SUDO systemctl daemon-reload
$SUDO systemctl enable --now markdown-wizard
$SUDO systemctl restart markdown-wizard
for i in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/api/status" > /dev/null && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$PORT/api/status"
echo
echo "-- service up on 127.0.0.1:$PORT"

# 5. The site: bootstrap first if there is no certificate yet, so
#    certbot has a live block; then the TLS site either way.
SITE_AVAIL=/etc/nginx/sites-available/$DOMAIN
SITE_ENABLED=/etc/nginx/sites-enabled/$DOMAIN
if [ ! -f "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" ]; then
  echo "-- no certificate yet: bootstrap site, then certbot"
  $SUDO install -m 0644 "$SRC/deploy/nginx-md.phext.io.bootstrap.conf" "$SITE_AVAIL"
  $SUDO ln -sf "$SITE_AVAIL" "$SITE_ENABLED"
  $SUDO nginx -t
  $SUDO systemctl reload nginx
  $SUDO certbot --nginx -d "$DOMAIN" --redirect --non-interactive --agree-tos \
    --register-unsafely-without-email || \
    $SUDO certbot certonly --nginx -d "$DOMAIN" --non-interactive
fi
echo "-- TLS site"
$SUDO install -m 0644 "$SRC/deploy/nginx-md.phext.io.conf" "$SITE_AVAIL"
$SUDO ln -sf "$SITE_AVAIL" "$SITE_ENABLED"
$SUDO nginx -t
$SUDO systemctl reload nginx

# 6. Smoke: the public name answers, through nginx, over HTTPS.
echo "-- smoke"
curl -sf "https://$DOMAIN/api/status"
echo
curl -sf "https://$DOMAIN/" > /dev/null && echo "editor answers at https://$DOMAIN/"
echo "== done: https://$DOMAIN =="
