# Build and test environment for Screenlet for GNOME: Rust and the GNOME
# libraries stay in this image (tools/dev.sh), nothing on the host.
FROM registry.fedoraproject.org/fedora:44
RUN dnf -y install --setopt=install_weak_deps=False \
        cargo rust clippy rustfmt gcc pkgconf-pkg-config \
        gtk4-devel libadwaita-devel gstreamer1-devel gstreamer1-plugins-base-devel \
        gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-ugly-free \
        gstreamer1-plugin-libav pipewire-gstreamer libusb1-devel openssl-devel \
        rpm-build dbus-daemon \
    && dnf clean all
ENV CARGO_HOME=/cargo
WORKDIR /src
