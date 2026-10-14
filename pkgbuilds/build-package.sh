#!/bin/bash

set -e
set -x

sudo mkdir -p /temp/package
sudo chown build:build /temp/package
sudo mkdir -p /workdir/aur-pkgs
sudo chown build:build /workdir/aur-pkgs

cp -r /workdir/pkgbuilds/${1}/. /temp/package/

PIKAUR_CMD="PKGDEST=/workdir/aur-pkgs pikaur --noconfirm --build-gpgdir /etc/pacman.d/gnupg -S -P /temp/package/PKGBUILD"
PIKAUR_RUN=(bash -c "${PIKAUR_CMD}")
"${PIKAUR_RUN[@]}"
