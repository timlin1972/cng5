#! /bin/bash

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
PURPLE='\033[0;35m'
NC='\033[0m' # No Color

function commit() {
    git add .
    git commit -m "m"
    git rebase -i HEAD~2
    git push -f
}

branch=$(git branch | sed -n -e 's/^\* \(.*\)/\1/p')

if [ "develop" = "${branch}" ]; then
    commit
else
    while true; do
    echo -e "The branch is ${RED}${branch}${NC} NOT ${GREEN}develop${NC}, commit?"
    read -p "(y/N) " yn
    yn=${yn:-N}
    echo $yn
    case $yn in
        [Yy]* ) commit; break;;
        [Nn]* ) exit;;
        * ) echo "Please answer yes or no.";;
    esac
done
fi

