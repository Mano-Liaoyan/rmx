#!/bin/sh
# Create the ephemeral user, force TLS and a credential-based autologon, then run xrdp.
set -eu
useradd -m -s /bin/sh "$RDP_USER"
printf '%s:%s\n' "$RDP_USER" "$RDP_PASS" | chpasswd

if [ "${RDP_SERVER:-xrdp}" = freerdp ]; then
    # FreeRDP's shadow server shares a virtual display and speaks NLA using this NT hash file.
    mkdir -p /etc/winpr
    winpr-hash3 -u "$RDP_USER" -p "$RDP_PASS" -f sam > /etc/winpr/SAM
    unset RDP_PASS
    Xvfb :0 -screen 0 1024x768x24 &
    sleep 1
    DISPLAY=:0 xterm -geometry 80x24+0+0 &
    DISPLAY=:0 exec freerdp-shadow-cli3 /port:3389 /sam-file:/etc/winpr/SAM
fi
unset RDP_PASS

ini=/etc/xrdp/xrdp.ini
sed -i -e 's/^security_layer=.*/security_layer=tls/' \
       -e 's/^#\?autorun=.*/autorun=Xorg/' \
       -e 's/^require_credentials=.*/require_credentials=true/' "$ini"
grep -q '^autorun=' "$ini" || sed -i '0,/^\[Globals\]/s//[Globals]\nautorun=Xorg/' "$ini"

mkdir -p /var/run/xrdp
/usr/sbin/xrdp-sesman --nodaemon &
exec /usr/sbin/xrdp --nodaemon
