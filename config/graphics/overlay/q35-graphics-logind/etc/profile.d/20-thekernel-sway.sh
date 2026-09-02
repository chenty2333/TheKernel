# A logind-created console session owns each graphics VT.  The dedicated
# gettys below authenticate through PAM before reaching this profile hook.
# Other gettys remain usable for recovery.
case "${USER:-}:$(tty 2>/dev/null || true):${THEKERNEL_SWAY_STARTED:-}" in
    alice:/dev/tty1:|bob:/dev/tty2:)
        export THEKERNEL_SWAY_STARTED=1
        exec /usr/local/bin/thekernel-sway-session
        ;;
esac
