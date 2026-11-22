#!/bin/sh
# Coda installer for any Linux distribution.
#
# 1. Checks every build and runtime dependency and lists what is missing.
# 2. Offers to install missing packages with your package manager
#    (pacman, apt, dnf, zypper, xbps), or rustup when the distro's Rust is too old.
# 3. Builds Coda, installs it, and adds icons and right-click menu entries.
#
# Usage: ./install.sh [--yes] [--system] [--check-only]
#   --yes         install missing dependencies without asking
#   --system      install the binary to /usr/local/bin (uses sudo) instead of ~/.local/bin
#   --check-only  only report dependency status, change nothing

set -eu

MIN_RUST="1.95"
ASSUME_YES=0
SYSTEM=0
CHECK_ONLY=0
for a in "$@"; do
    case "$a" in
        --yes|-y) ASSUME_YES=1 ;;
        --system) SYSTEM=1 ;;
        --check-only) CHECK_ONLY=1 ;;
        -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $a"; exit 2 ;;
    esac
done

cd "$(dirname "$0")"

if [ -t 1 ]; then B="$(printf '\033[1m')"; G="$(printf '\033[32m')"; Y="$(printf '\033[33m')"; R="$(printf '\033[31m')"; N="$(printf '\033[0m')"; else B=""; G=""; Y=""; R=""; N=""; fi
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$1"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$1"; }
bad()  { printf '  %s✗%s %s\n' "$R" "$N" "$1"; }

ask() {
    [ "$ASSUME_YES" = 1 ] && return 0
    [ -t 0 ] || return 1
    printf '%s [Y/n] ' "$1"
    read -r reply
    case "$reply" in [nN]*) return 1 ;; *) return 0 ;; esac
}

[ "$(id -u)" = 0 ] && SUDO="" || SUDO="sudo"

# ------------------------------------------------------------------ distro

DISTRO="unknown"
if [ -r /etc/os-release ]; then
    . /etc/os-release
    for id in ${ID:-} ${ID_LIKE:-}; do
        case "$id" in
            arch|cachyos|endeavouros|manjaro|garuda) DISTRO=arch; break ;;
            debian|ubuntu|linuxmint|pop) DISTRO=debian; break ;;
            fedora|rhel|centos|nobara) DISTRO=fedora; break ;;
            opensuse*|suse|sles) DISTRO=suse; break ;;
            void) DISTRO=void; break ;;
        esac
    done
fi

# package name for each dependency key, per distro
pkg() {
    case "$DISTRO:$1" in
        arch:rust) echo rust ;;              arch:cc) echo base-devel ;;
        arch:pkgconfig) echo pkgconf ;;      arch:ffmpeg) echo ffmpeg ;;
        arch:fontconfig) echo fontconfig ;;  arch:xkbcommon) echo libxkbcommon ;;
        arch:wayland) echo wayland ;;        arch:egl) echo libglvnd ;;
        arch:portal) echo xdg-desktop-portal ;; arch:notify) echo libnotify ;;

        debian:rust) echo "cargo rustc" ;;   debian:cc) echo build-essential ;;
        debian:pkgconfig) echo pkg-config ;; debian:ffmpeg) echo ffmpeg ;;
        debian:fontconfig) echo libfontconfig-dev ;; debian:xkbcommon) echo libxkbcommon0 ;;
        debian:wayland) echo libwayland-dev ;; debian:egl) echo libegl1 ;;
        debian:portal) echo xdg-desktop-portal ;; debian:notify) echo libnotify-bin ;;

        fedora:rust) echo "cargo rust" ;;    fedora:cc) echo gcc ;;
        fedora:pkgconfig) echo pkgconf-pkg-config ;; fedora:ffmpeg) echo ffmpeg-free ;;
        fedora:fontconfig) echo fontconfig-devel ;; fedora:xkbcommon) echo libxkbcommon-devel ;;
        fedora:wayland) echo wayland-devel ;; fedora:egl) echo mesa-libEGL ;;
        fedora:portal) echo xdg-desktop-portal ;; fedora:notify) echo libnotify ;;

        suse:rust) echo "cargo rust" ;;      suse:cc) echo gcc ;;
        suse:pkgconfig) echo pkg-config ;;   suse:ffmpeg) echo ffmpeg ;;
        suse:fontconfig) echo fontconfig-devel ;; suse:xkbcommon) echo libxkbcommon-devel ;;
        suse:wayland) echo wayland-devel ;;  suse:egl) echo Mesa-libEGL1 ;;
        suse:portal) echo xdg-desktop-portal ;; suse:notify) echo libnotify-tools ;;

        void:rust) echo "rust cargo" ;;      void:cc) echo gcc ;;
        void:pkgconfig) echo pkg-config ;;   void:ffmpeg) echo ffmpeg ;;
        void:fontconfig) echo fontconfig-devel ;; void:xkbcommon) echo libxkbcommon-devel ;;
        void:wayland) echo wayland-devel ;;  void:egl) echo libEGL ;;
        void:portal) echo xdg-desktop-portal ;; void:notify) echo libnotify ;;
        *) echo "" ;;
    esac
}

install_pkgs() {
    case "$DISTRO" in
        arch)   $SUDO pacman -S --needed --noconfirm "$@" ;;
        debian) $SUDO apt-get update && $SUDO apt-get install -y "$@" ;;
        fedora) $SUDO dnf install -y "$@" ;;
        suse)   $SUDO zypper --non-interactive install "$@" ;;
        void)   $SUDO xbps-install -Sy "$@" ;;
        *) return 1 ;;
    esac
}

version_ge() { # $1 >= $2 ?
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

have_lib() { command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$1"; }

have_so() { # runtime shared library present?
    ldconfig -p 2>/dev/null | grep -q "$1" && return 0
    for d in /usr/lib /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu; do
        ls "$d"/$1* >/dev/null 2>&1 && return 0
    done
    return 1
}

have_egl() {
    ldconfig -p 2>/dev/null | grep -q 'libEGL\.so' && return 0
    for d in /usr/lib /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu; do
        ls "$d"/libEGL.so* >/dev/null 2>&1 && return 0
    done
    return 1
}

# ------------------------------------------------------------------ checks

MISSING=""        # required keys
MISSING_OPT=""    # optional keys
RUST_TOO_OLD=0

check_all() {
    MISSING=""; MISSING_OPT=""; RUST_TOO_OLD=0
    printf '%sBuild dependencies%s\n' "$B" "$N"
    if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
        rv="$(rustc --version | awk '{print $2}')"
        if version_ge "$rv" "$MIN_RUST"; then ok "Rust $rv"
        else bad "Rust $rv is older than the required $MIN_RUST"; RUST_TOO_OLD=1; MISSING="$MISSING rust"; fi
    else bad "Rust (cargo + rustc) $MIN_RUST or newer"; MISSING="$MISSING rust"; fi
    if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; then ok "C compiler"; else bad "C compiler (gcc/cc)"; MISSING="$MISSING cc"; fi
    if command -v pkg-config >/dev/null 2>&1; then ok "pkg-config"; else bad "pkg-config"; MISSING="$MISSING pkgconfig"; fi
    if have_lib fontconfig; then ok "fontconfig (development files)"; else bad "fontconfig (development files)"; MISSING="$MISSING fontconfig"; fi

    printf '%sRuntime dependencies%s\n' "$B" "$N"
    if command -v ffmpeg >/dev/null 2>&1; then
        enc="$(ffmpeg -hide_banner -encoders 2>/dev/null || true)"
        ok "ffmpeg $(ffmpeg -version 2>/dev/null | awk 'NR==1{print $3}')"
        for e in libmp3lame:MP3 libopus:Opus libvorbis:Vorbis wmav2:WMA wavpack:WavPack; do
            name="${e%%:*}"; label="${e##*:}"
            echo "$enc" | grep -q " $name " && ok "  $label encoder ($name)" || warn "  $label encoder ($name) missing from this ffmpeg build; $label output will not work"
        done
    else bad "ffmpeg"; MISSING="$MISSING ffmpeg"; fi
    if have_lib xkbcommon || have_so libxkbcommon.so; then ok "libxkbcommon (keyboard input)"; else bad "libxkbcommon (keyboard input)"; MISSING="$MISSING xkbcommon"; fi
    if have_lib wayland-client || ldconfig -p 2>/dev/null | grep -q libwayland-client; then ok "Wayland client library"; else warn "Wayland client library (needed on Wayland sessions)"; MISSING_OPT="$MISSING_OPT wayland"; fi
    if have_egl; then ok "EGL/OpenGL (hardware-accelerated drawing)"; else warn "EGL/OpenGL not found (Coda falls back to software drawing)"; MISSING_OPT="$MISSING_OPT egl"; fi
    if [ -x /usr/lib/xdg-desktop-portal ] || [ -x /usr/libexec/xdg-desktop-portal ] || command -v xdg-desktop-portal >/dev/null 2>&1; then ok "xdg-desktop-portal (file dialogs)"; else warn "xdg-desktop-portal (needed for the Add files / Browse dialogs)"; MISSING_OPT="$MISSING_OPT portal"; fi
    if command -v notify-send >/dev/null 2>&1; then ok "notify-send (notifications for right-click conversions)"; else warn "notify-send (optional: notifications for right-click conversions)"; MISSING_OPT="$MISSING_OPT notify"; fi

    printf '%sCD drive access%s\n' "$B" "$N"
    found=0
    for d in /dev/sr*; do
        [ -e "$d" ] || continue
        found=1
        if [ -r "$d" ]; then ok "$d is readable"; else
            grp="$(stat -c %G "$d" 2>/dev/null || echo optical)"
            warn "$d is not readable by $(id -un); add yourself to the '$grp' group: sudo usermod -aG $grp $(id -un) (then log out and in)"
        fi
    done
    [ "$found" = 1 ] || warn "No optical drive detected right now (the converter works without one)"
}

check_all

if [ "$CHECK_ONLY" = 1 ]; then
    [ -z "$MISSING" ] && { echo "All required dependencies are present."; exit 0; } || exit 1
fi

# ------------------------------------------------------------------ resolve

if [ -n "$MISSING$MISSING_OPT" ]; then
    echo
    # Rust that is merely too old on Debian/Ubuntu/openSUSE is best fixed with rustup.
    use_rustup=0
    if [ "$RUST_TOO_OLD" = 1 ] && [ "$DISTRO" != arch ] && [ "$DISTRO" != void ] && [ "$DISTRO" != fedora ]; then use_rustup=1; fi
    if [ "$DISTRO" = unknown ] && echo "$MISSING" | grep -q rust; then use_rustup=1; fi

    pkgs=""
    for k in $MISSING $MISSING_OPT; do
        [ "$k" = rust ] && [ "$use_rustup" = 1 ] && continue
        pkgs="$pkgs $(pkg "$k")"
    done
    pkgs="$(echo $pkgs)"

    if [ -n "$pkgs" ]; then
        if [ "$DISTRO" = unknown ]; then
            echo "Your distribution wasn't recognised. Please install the items marked above with your package manager, then run this script again."
            [ -n "$MISSING" ] && exit 1
        elif ask "Install with your package manager:$B $pkgs$N ?"; then
            install_pkgs $pkgs || { echo "${R}Package installation failed.${N}"; [ -n "$MISSING" ] && exit 1; }
            [ "$DISTRO" = fedora ] && echo "$MISSING" | grep -q ffmpeg && \
                echo "Note: Fedora's ffmpeg-free lacks some codecs. For the full build enable RPM Fusion and run: sudo dnf swap ffmpeg-free ffmpeg --allowerasing"
        elif [ -n "$MISSING" ]; then
            echo "Required dependencies are missing; stopping."; exit 1
        fi
    fi

    if [ "$use_rustup" = 1 ]; then
        if command -v rustup >/dev/null 2>&1; then
            ask "Update Rust with rustup to the latest stable?" && rustup update stable && rustup default stable
        elif ask "Install a current Rust toolchain for your user with rustup (https://rustup.rs)?"; then
            command -v curl >/dev/null 2>&1 || { echo "curl is needed to fetch rustup; install curl and retry."; exit 1; }
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        else
            echo "Rust $MIN_RUST or newer is required; stopping."; exit 1
        fi
        [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    fi

    echo
    echo "${B}Re-checking…${N}"
    check_all
    [ -z "$MISSING" ] || { echo "${R}Still missing:$MISSING${N}"; exit 1; }
fi

# ------------------------------------------------------------------ build & install

echo
echo "${B}Building Coda (this takes a few minutes the first time)…${N}"
cargo build --release --locked

if [ "$SYSTEM" = 1 ]; then
    BIN=/usr/local/bin/coda
    $SUDO install -Dm755 target/release/coda "$BIN"
else
    BIN="$HOME/.local/bin/coda"
    install -Dm755 target/release/coda "$BIN"
fi
"$BIN" install-integration

echo
echo "${G}Coda installed to $BIN${N}"
case ":$PATH:" in
    *":$(dirname "$BIN"):"*) ;;
    *) warn "$(dirname "$BIN") is not on your PATH; add it to run 'coda' from a terminal (the app menu entry works regardless)." ;;
esac
echo "Start it from your application menu, or right-click music files and choose Coda."
