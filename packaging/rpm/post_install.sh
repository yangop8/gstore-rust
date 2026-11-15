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
# >>> first-start root password (tests/packaging_first_start.rs runs this
# block). A fresh install gets its own random root password instead of
# upstream's shared default 123456, which gserver refuses to serve (conf.ini's
# allow_default_root_password). It goes to a file of its own,
# GSTORE_PKG_PWFILE (0640 root:gstore), never into conf.ini (the package's
# configuration file, the administrator's from now on). The systemd unit
# hands the file to gserver (GSTORE_ROOT_PASSWORD_FILE), which seeds the user
# store with it at the first start. An upgrade, a server that already has a
# user store, or a conf.ini with a password of its own is left as it is. The
# paths and the group can be overridden for tests. (No `set -e` in an rpm
# scriptlet: every step's status is checked.)
GSTORE_PKG_CONF=${GSTORE_PKG_CONF:-/etc/gstore/conf.ini}
GSTORE_PKG_DATA=${GSTORE_PKG_DATA:-/var/lib/gstore}
GSTORE_PKG_PWFILE=${GSTORE_PKG_PWFILE:-/etc/gstore/root_password}
GSTORE_PKG_GROUP=${GSTORE_PKG_GROUP:-gstore}
if [ "$1" = 1 ] && [ ! -e "$GSTORE_PKG_DATA/dbhome/users.db" ] \
    && [ ! -e "$GSTORE_PKG_PWFILE" ] \
    && grep -q '^[[:space:]]*root_password[[:space:]]*=[[:space:]]*"\{0,1\}123456"\{0,1\}[[:space:]]*$' \
        "$GSTORE_PKG_CONF"; then
    gstore_pw=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n') || gstore_pw=
    gstore_tmp="$GSTORE_PKG_PWFILE.gstore-new"
    # Written beside the target, private until it is complete, then given
    # its group and mode, synced and renamed into place: the file is either
    # absent or whole.
    if [ ${#gstore_pw} -eq 32 ] \
        && ( umask 077; printf '%s\n' "$gstore_pw" >"$gstore_tmp" ) \
        && chgrp "$GSTORE_PKG_GROUP" "$gstore_tmp" \
        && chmod 0640 "$gstore_tmp" \
        && { sync "$gstore_tmp" 2>/dev/null || sync; } \
        && mv -f "$gstore_tmp" "$GSTORE_PKG_PWFILE"; then
        echo "gstore: root's initial password is a random one, in $GSTORE_PKG_PWFILE" \
            "(readable by root and the gstore group); gserver sets it when it creates the" \
            "user store at its first start. Change it after the first start (gconsole" \
            "setpswd, or the /api userPassword operation), then delete the file."
    else
        rm -f "$gstore_tmp"
        echo "gstore: could not set a random root password: set root_password in" \
            "$GSTORE_PKG_CONF before the first start" >&2
    fi
    unset gstore_pw gstore_tmp
fi
# <<< first-start root password
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
