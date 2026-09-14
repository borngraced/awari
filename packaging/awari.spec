Name:           awari
Version:        @VERSION@
Release:        1%{?dist}
Summary:        A fast Wayland launcher for apps, files and windows

License:        GPL-3.0-or-later
URL:            https://github.com/borngraced/awari
Source0:        awari
Source1:        awari.service
Source2:        config.kdl
Source3:        LICENSE

ExclusiveArch:  x86_64

Requires:       wayland
Requires:       mesa-libEGL
Requires:       fontconfig
Requires:       libxkbcommon

%description
Awari is a Wayland launcher built on GPUI and fff search. A GPU-free daemon
ranks windows, apps and files in one fuzzy list, with an in-process search so
there is no per-keystroke subprocess.

%install
install -D -m755 %{SOURCE0} %{buildroot}%{_bindir}/awari
install -D -m644 %{SOURCE1} %{buildroot}/usr/lib/systemd/user/awari.service
install -D -m644 %{SOURCE2} %{buildroot}%{_datadir}/awari/config.kdl
install -D -m644 %{SOURCE3} %{buildroot}%{_datadir}/licenses/%{name}/LICENSE

%files
%{_bindir}/awari
/usr/lib/systemd/user/awari.service
%{_datadir}/awari/config.kdl
%{_datadir}/licenses/%{name}/LICENSE

%changelog
* Sun Sep 14 2026 Samuel Onoja Taiwo - @VERSION@-1
- Initial release