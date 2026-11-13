# Arch Linux package (AUR: micdock-bin)

`PKGBUILD` repackages the release archives from GitHub (x86_64 and aarch64), so installing takes seconds
instead of compiling libcosmic. It installs:

- `micdock`, `ugg2wav`, `rodego2` and `micdock-firmware` in `/usr/bin`;
- the app-menu entry and the manual (`/usr/share/micdock/docs`, which the Help page reads);
- the udev rule in `/usr/lib/udev/rules.d`, so the logged-in user can reach the units without sudo (pacman
  reloads the rules; replug the receiver once).

`ffmpeg` is an optional dependency, needed only for MP3 export.

Users install it with an AUR helper (`yay -S micdock-bin`, `paru -S micdock-bin`) or by hand:
`git clone https://aur.archlinux.org/micdock-bin.git && cd micdock-bin && makepkg -si`.

## Publishing

One-time setup: add your SSH public key to your AUR account (*My Account → SSH Public Key*), then:

```bash
git clone ssh://aur@aur.archlinux.org/micdock-bin.git ~/aur/micdock-bin   # empty on the first clone
```

For every release, after the `v<version>` release is on GitHub:

```bash
packaging/arch/update.sh <version>        # pkgver, checksums, test build in an Arch container, .SRCINFO
cp packaging/arch/PKGBUILD packaging/arch/.SRCINFO ~/aur/micdock-bin/
cd ~/aur/micdock-bin && git add PKGBUILD .SRCINFO && git commit -m "Update to <version>" && git push
```

The first push creates the package on aur.archlinux.org. The AUR only accepts the `master` branch, and every
commit there must contain an up-to-date `.SRCINFO`.

If only the packaging changes (not the release), raise `pkgrel` in the PKGBUILD, run
`packaging/arch/update.sh --srcinfo` and push the same way.

Commit the updated `PKGBUILD` and `.SRCINFO` here too, so this folder matches what is on the AUR.
