# %post of the gstore .rpm (cargo-generate-rpm `post_install_script`): create
# the gstore system user and /var/lib/gstore, let the user read the
# configuration, and make systemd see the unit. The service is not enabled or
# started automatically: `systemctl enable --now gserver` (docs/DEPLOY.md).
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers gstore.conf || :
elif ! getent passwd gstore >/dev/null; then
    useradd --system --user-group --home-dir /var/lib/gstore --no-create-home \
        --shell /sbin/nologin --comment "gStore database server" gstore || :
fi
if command -v systemd-tmpfiles >/dev/null 2>&1; then
    systemd-tmpfiles --create gstore.conf || :
else
    mkdir -p /var/lib/gstore
    chown gstore:gstore /var/lib/gstore
    chmod 0750 /var/lib/gstore
    [ -e /var/lib/gstore/conf ] || ln -s /etc/gstore /var/lib/gstore/conf
fi
chgrp gstore /etc/gstore /etc/gstore/conf.ini || :
chmod 0750 /etc/gstore || :
chmod 0640 /etc/gstore/conf.ini || :
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
    # $1 > 1: an upgrade; restart a running server on the new binaries.
    if [ "$1" -gt 1 ] 2>/dev/null; then
        systemctl try-restart gserver.service >/dev/null 2>&1 || :
    fi
fi
