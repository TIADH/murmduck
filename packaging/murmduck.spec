%global crate murmduck

Name:           murmduck
Version:        0.1.0
Release:        1%{?dist}
Summary:        On-demand streaming text-to-speech for Linux
License:        MIT OR Apache-2.0
URL:            https://github.com/TIADH/murmduck
# Produced by packaging/make-source-tarball.sh: the crate sources, Cargo.lock,
# LICENSE, docs, and a vendor/ tree plus .cargo/config.toml for an offline build.
Source0:        %{crate}-%{version}.tar.gz

BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  pkgconf-pkg-config
BuildRequires:  gstreamer1-devel
BuildRequires:  gstreamer1-plugins-base-devel
# %check decodes the bundled MP3 fixture, so the decoder/sink plugins must be
# present in the buildroot too, not only at runtime.
BuildRequires:  gstreamer1-plugins-good
BuildRequires:  gstreamer1-plugins-ugly-free

Requires:       wl-clipboard
Requires:       gstreamer1
Requires:       gstreamer1-plugins-base
Requires:       gstreamer1-plugins-good
# gstreamer1-plugins-ugly-free provides mpg123audiodec on Fedora. If a Fedora
# release ships that element in a different plugin package, adjust this
# requirement accordingly; the pipeline needs MPEG-1 Layer III decoding.
Requires:       gstreamer1-plugins-ugly-free

%description
Murmduck reads the PRIMARY selection aloud through an OpenAI-compatible
streaming TTS server. It is a single, short-lived CLI: a KDE global shortcut
runs "murmduck toggle" to start reading, and the same shortcut stops it.
There is no daemon, systemd service or GUI.

%prep
%autosetup -n %{crate}-%{version}

%build
export CARGO_TARGET_DIR=%{_builddir}/murmduck-target
cargo build --release --locked --offline

%install
install -D -m 0755 %{_builddir}/murmduck-target/release/murmduck \
    %{buildroot}%{_bindir}/murmduck
install -D -m 0644 LICENSE \
    %{buildroot}%{_datadir}/licenses/%{name}/LICENSE
install -D -m 0644 README.md \
    %{buildroot}%{_datadir}/doc/%{name}/README.md
install -D -m 0644 docs/installation.md \
    %{buildroot}%{_datadir}/doc/%{name}/installation.md
install -D -m 0644 docs/acceptance-report.md \
    %{buildroot}%{_datadir}/doc/%{name}/acceptance-report.md

%check
export CARGO_TARGET_DIR=%{_builddir}/murmduck-target
cargo test --release --locked --offline

%files
%license LICENSE
%doc README.md docs/installation.md docs/acceptance-report.md
%{_bindir}/murmduck

%changelog
* Wed Nov 18 2026 Murmduck contributors <noreply@example.com> - 0.1.0-1
- Initial MVP package: CLI, config, PRIMARY selection, single-instance
  coordination, streaming GStreamer playback and KDE shortcut documentation.
