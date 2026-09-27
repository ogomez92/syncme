#!/bin/sh
# Installs dist/syncme and the systemd service, then starts SyncMe for a user.
# Usage: sudo sh scripts/install-linux.sh [USERNAME]   (defaults to $SUDO_USER)
set -e
USERNAME=${1:-$SUDO_USER}
if [ -z "$USERNAME" ]; then
  echo "usage: sudo sh $0 USERNAME" >&2
  exit 1
fi
[ -f dist/syncme ] || sh scripts/build-linux.sh
install -m 755 dist/syncme /usr/local/bin/syncme
install -m 644 packaging/linux/syncme@.service /etc/systemd/system/syncme@.service
systemctl daemon-reload
systemctl enable --now "syncme@$USERNAME"
systemctl --no-pager status "syncme@$USERNAME" | head -5
echo "SyncMe is running for $USERNAME. Logs: journalctl -u syncme@$USERNAME -f"
