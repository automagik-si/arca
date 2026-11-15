#!/bin/bash
# invoke_tests in record mode (see patch-template.py): runs GitHub's own helper unchanged. A failed test is written to
# the list that is published with the image, and the build goes on.
export HELPER_SCRIPTS="${HELPER_SCRIPTS:-/imagegeneration/helpers}"
if ! "$HELPER_SCRIPTS/invoke-tests.sh" "$@"; then
  printf '%s\n' "$1${2:+ / $2}" >>/usr/local/lib/arca-image/test-failures.log
  echo "arca: test '$1${2:+ / $2}' failed; recorded, the build goes on"
fi
exit 0
