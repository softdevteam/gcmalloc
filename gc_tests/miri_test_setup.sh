#!/bin/sh
set -e;

PROJECTDIR=$1
TMPDIR=$2

# Miri deps cause breakages when used with non-miri targets. So we copy gcmalloc
# sources over to a tempdir to compile with miri flags so we can nuke it
# afterwards.
rsync -a --partial --info=progress2 --exclude .git --exclude target --exclude gc_tests $PROJECTDIR/ $TMPDIR

mkdir -p "$TMPDIR/src/bin/"

# Running this dummy binary forces cargo-miri to compile gcmalloc with miri
# flags as a dependency.
echo "extern crate gcmalloc; fn main() {}" > $TMPDIR/src/bin/miri.rs
(cd $TMPDIR && cargo-miri miri setup)
(cd $TMPDIR && cargo-miri miri run -- -Zmiri-ignore-leaks)

rm $TMPDIR/src/bin/miri.rs
