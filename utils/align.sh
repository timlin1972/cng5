#! /bin/bash

cng4="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cd "$cng4"

cd ..
/usr/bin/git fetch --all
/usr/bin/git reset --hard origin/main