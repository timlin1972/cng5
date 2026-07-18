#! /bin/bash

cng4="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while true
do
        cd "$cng4"

        cd ..
        /usr/bin/git fetch --all
        /usr/bin/git reset --hard origin/main

        # cd client
        # npm install
        # npm run build
        # cd ..

        # cd server
        CARGO_BIN=$(which cargo)

        if [ -z "$CARGO_BIN" ]; then
                echo "cargo not found in PATH"
                exit 1
        fi

        "$CARGO_BIN" run
        # cd ..
done
