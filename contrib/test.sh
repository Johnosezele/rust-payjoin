#!/usr/bin/env bash
set -e

LOCKFILE="Cargo.lock"
LOCKFILE_BAK="Cargo.lock.bak"
LOCKDIR=".Cargo.lock.lock"

cleanup() {
    if [ -f "$LOCKFILE_BAK" ]; then
        mv "$LOCKFILE_BAK" "$LOCKFILE" 2>/dev/null || true
    fi
    rmdir "$LOCKDIR" 2>/dev/null || true
}

for sig in EXIT INT TERM HUP; do
    trap "cleanup; trap - $sig EXIT; kill -$sig $$" $sig
done
if ! mkdir "$LOCKDIR" 2>/dev/null; then
    echo "Another instance is running. If you're sure it's not, remove $LOCKDIR and try again." >&2
    exit 1
fi

if [ -f "$LOCKFILE" ]; then
    mv "$LOCKFILE" "$LOCKFILE_BAK"
fi

DEPS="recent minimal"
CRATES="payjoin payjoin-cli payjoin-directory payjoin-ffi"

for dep in $DEPS; do
    cargo --version
    rustc --version

    # Some tests require certain toolchain types.
    export NIGHTLY=false
    export STABLE=true
    if cargo --version | grep nightly; then
        STABLE=false
        NIGHTLY=true
    fi

    cp "Cargo-$dep.lock" Cargo.lock

    for crate in $CRATES; do
        (
            cd "$crate"
            ./contrib/test.sh
        )
    done
done

