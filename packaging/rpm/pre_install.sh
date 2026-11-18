# %pre of the gstore .rpm (cargo-generate-rpm `pre_install_script`): create
# the gstore system user and group before rpm lays down the files, so that
# /etc/gstore/conf.ini (0640) gets the group gstore the package header names
# (review infra#15: it used to be installed root:root and fixed up by %post,
# so `rpm -V gstore` reported a group mismatch and `rpm --setugids gstore`
# made conf.ini unreadable to gserver).
# The sysusers.d file is not installed yet, so its line is given here (the
# compat form of Fedora's %sysusers_create_compat; the same line as
# packaging/sysusers/gstore.conf, which tests/packaging_scripts.rs checks).
# No `set -e` in an rpm scriptlet: every step's status is checked.
if ! getent group gstore >/dev/null 2>&1 || ! getent passwd gstore >/dev/null 2>&1; then
    if command -v systemd-sysusers >/dev/null 2>&1; then
        printf '%s\n' 'u gstore - "gStore database server" /var/lib/gstore /usr/sbin/nologin' \
            | systemd-sysusers --replace=/usr/lib/sysusers.d/gstore.conf - || :
    fi
    if ! getent group gstore >/dev/null 2>&1; then
        groupadd --system gstore || :
    fi
    if ! getent passwd gstore >/dev/null 2>&1; then
        useradd --system --gid gstore --home-dir /var/lib/gstore --no-create-home \
            --shell /sbin/nologin --comment "gStore database server" gstore || :
    fi
fi
getent group gstore >/dev/null 2>&1 \
    || echo "gstore: could not create the gstore group: /etc/gstore/conf.ini is installed with group root" >&2
