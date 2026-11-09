# Packs the tree that build.sh staged, so nothing is compiled here and the binary stays as cargo
# left it. rmx_version and rmx_payload come from build.sh.
%global debug_package %{nil}
%global _build_id_links none
%global __os_install_post %{nil}
# xz, which every rpm in use can read.
%global _binary_payload w6.xzdio

Name:           rmx
Version:        %{rmx_version}
Release:        1
Summary:        Native remote connection manager
License:        Apache-2.0
URL:            https://github.com/Mano-Liaoyan/rmx

# rpm generates the requirements for libraries the binary links. These are the ones it opens at
# run time, which rpm cannot see: EGL or GLX, then the Wayland or X11 client libraries. Sonames
# rather than package names, because the names differ between openSUSE, Fedora, and RHEL.
Requires:       libEGL.so.1()(64bit)
Requires:       libGL.so.1()(64bit)
Requires:       libwayland-client.so.0()(64bit)
Requires:       libwayland-egl.so.1()(64bit)
Requires:       libxkbcommon.so.0()(64bit)
Requires:       libxkbcommon-x11.so.0()(64bit)
Requires:       libX11.so.6()(64bit)
Requires:       libX11-xcb.so.1()(64bit)
Requires:       libxcb.so.1()(64bit)
Requires:       libXcursor.so.1()(64bit)
Requires:       libXi.so.6()(64bit)
Requires:       libXrender.so.1()(64bit)
# Owns the icon directories, so that they are not left behind on removal.
Requires:       hicolor-icon-theme
# rmx exits without a font for fontconfig's "monospace", and minimal systems have none. Debian's
# libfontconfig1 pulls one in already.
Requires:       font(dejavusansmono)
# fc-list, which finds a CJK fallback font.
Recommends:     fontconfig

%description
rmx connects to remote machines over SSH, Telnet, serial, VNC, and RDP from one
window, with an encrypted vault for its connections.

%install
cp -a %{rmx_payload}/. %{buildroot}/

%post
touch --no-create /usr/share/icons/hicolor 2>/dev/null || :

%postun
touch --no-create /usr/share/icons/hicolor 2>/dev/null || :

%files
/usr/bin/rmx
/usr/share/applications/rmx.desktop
/usr/share/icons/hicolor/*/apps/rmx.*
%dir /usr/share/licenses/rmx
%license /usr/share/licenses/rmx/*
