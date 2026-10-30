#!/bin/sh
# Builds Screenlet and packages it: tools/package.sh deb|rpm|install [DESTDIR]
#   deb, rpm  -> dist/screenlet_<version>_<arch>.deb or dist/screenlet-<version>.<arch>.rpm
#   install   -> into DESTDIR (default /), for a source install (as root)
set -e
cd "$(dirname "$0")/.."
kind="${1:?deb, rpm or install}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$SKIP_BUILD" ] || cargo build --release --locked
stage="${2:-$PWD/dist/stage}"
[ "$kind" = install ] || rm -rf "$stage"
d() { install -d "$stage$1"; }
f() { install -D -m "$1" "$2" "$stage$3"; }
f 755 target/release/screenlet /usr/bin/screenlet
f 755 target/release/screenlet-pen-helper /usr/libexec/screenlet-pen-helper
f 644 data/io.github.davideloconte.Screenlet.desktop /usr/share/applications/io.github.davideloconte.Screenlet.desktop
f 644 data/io.github.davideloconte.Screenlet.service /usr/share/dbus-1/services/io.github.davideloconte.Screenlet.service
f 644 data/pen-helper/io.github.davideloconte.Screenlet.Pen.service /usr/share/dbus-1/system-services/io.github.davideloconte.Screenlet.Pen.service
f 644 data/pen-helper/io.github.davideloconte.Screenlet.Pen.conf /usr/share/dbus-1/system.d/io.github.davideloconte.Screenlet.Pen.conf
f 644 data/pen-helper/io.github.davideloconte.screenlet.policy /usr/share/polkit-1/actions/io.github.davideloconte.screenlet.policy
f 644 data/70-screenlet.rules /usr/lib/udev/rules.d/70-screenlet.rules
f 644 data/icons/io.github.davideloconte.Screenlet.svg /usr/share/icons/hicolor/scalable/apps/io.github.davideloconte.Screenlet.svg
f 644 data/icons/io.github.davideloconte.Screenlet-symbolic.svg /usr/share/icons/hicolor/symbolic/apps/io.github.davideloconte.Screenlet-symbolic.svg
ext=data/gnome-shell/screenlet@davideloconte.github.io
for file in "$ext"/*; do f 644 "$file" "/usr/share/gnome-shell/extensions/screenlet@davideloconte.github.io/$(basename "$file")"; done
d /usr/lib/modules-load.d
echo uhid > "$stage/usr/lib/modules-load.d/screenlet.conf"   # the pen helper's /dev/uhid
f 644 LICENSE /usr/share/doc/screenlet/LICENSE
mkdir -p dist
case "$kind" in
install)
    modprobe uhid 2>/dev/null || true
    udevadm control --reload 2>/dev/null || true
    echo "installed into ${2:-/}" ;;
deb)
    arch="$(dpkg --print-architecture)"
    d /DEBIAN
    cat > "$stage/DEBIAN/control" <<CONTROL
Package: screenlet
Version: $version
Architecture: $arch
Maintainer: Davide Loconte <davide.loconte21@gmail.com>
Depends: libgtk-4-1 (>= 4.14), libadwaita-1-0 (>= 1.5), libgstreamer1.0-0, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-pipewire, libusb-1.0-0, libssl3t64 | libssl3, policykit-1 | polkitd
Recommends: gstreamer1.0-plugins-ugly, network-manager
Section: utils
Priority: optional
Homepage: https://github.com/DavideLoconte/Screenlet-gnome
Description: Android tablet as a low-latency second screen for GNOME
 Extends or mirrors the desktop on an Android tablet running Screenlet
 Companion, with touch and pen, over a USB cable, the network or Wi-Fi Direct.
CONTROL
    printf '#!/bin/sh\nset -e\nmodprobe uhid 2>/dev/null || true\nudevadm control --reload 2>/dev/null || true\nudevadm trigger --subsystem-match=usb 2>/dev/null || true\n' > "$stage/DEBIAN/postinst"
    chmod 755 "$stage/DEBIAN/postinst"
    dpkg-deb --root-owner-group --build "$stage" "dist/screenlet_${version}_${arch}.deb" ;;
rpm)
    arch="$(uname -m)"
    top="$PWD/dist/rpm"
    rm -rf "$top" && mkdir -p "$top/SPECS"
    files="$(cd "$stage" && find . -type f | sed 's#^\.##')"
    cat > "$top/SPECS/screenlet.spec" <<SPEC
Name: screenlet
Version: $version
Release: 1
Summary: Android tablet as a low-latency second screen for GNOME
License: MIT
URL: https://github.com/DavideLoconte/Screenlet-gnome
Requires: gtk4 >= 4.14, libadwaita >= 1.5, gstreamer1-plugins-base, gstreamer1-plugins-good, gstreamer1-plugins-bad-free, pipewire-gstreamer, libusb1, openssl-libs, polkit
Recommends: gstreamer1-plugins-ugly-free, NetworkManager
%description
Extends or mirrors the desktop on an Android tablet running Screenlet
Companion, with touch and pen, over a USB cable, the network or Wi-Fi Direct.
%install
cp -a "$stage/." %{buildroot}/
%post
modprobe uhid 2>/dev/null || :
udevadm control --reload 2>/dev/null || :
udevadm trigger --subsystem-match=usb 2>/dev/null || :
%files
$files
SPEC
    rpmbuild -bb --define "_topdir $top" --define "_build_id_links none" "$top/SPECS/screenlet.spec" >/dev/null
    cp "$top/RPMS/$arch/screenlet-$version-1.$arch.rpm" dist/ ;;
esac
ls dist/*.deb dist/*.rpm 2>/dev/null || true
