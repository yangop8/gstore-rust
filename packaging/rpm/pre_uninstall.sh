# %preun of the gstore .rpm: on erase ($1 = 0, not an upgrade) stop the server
# (gracefully: SIGTERM saves unsaved changes) before its files are removed.
# The databases in /var/lib/gstore are kept.
if [ "$1" -eq 0 ] 2>/dev/null && [ -d /run/systemd/system ]; then
    systemctl stop gserver.service >/dev/null 2>&1 || :
    systemctl disable gserver.service >/dev/null 2>&1 || :
fi
