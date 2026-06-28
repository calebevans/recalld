#!/usr/bin/env bash
set -euo pipefail

REPO="calebevans/recalld"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

info() { printf "\033[1;34m==>\033[0m %s\n" "$1"; }
warn() { printf "\033[1;33m==>\033[0m %s\n" "$1"; }
error() { printf "\033[1;31merror:\033[0m %s\n" "$1" >&2; exit 1; }

detect_platform() {
    local os arch

    os="$(uname -s)"
    case "$os" in
        Linux)  os="linux" ;;
        Darwin) os="darwin" ;;
        *)      error "Unsupported OS: $os. Only Linux and macOS are supported." ;;
    esac

    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *)              error "Unsupported architecture: $arch. Only x86_64 and aarch64 are supported." ;;
    esac

    PLATFORM="${arch}-${os}"
}

get_installed_version() {
    if [ -x "${INSTALL_DIR}/recalld" ]; then
        INSTALLED_VERSION="$("${INSTALL_DIR}/recalld" --version 2>/dev/null | awk '{print $2}' || echo "")"
    else
        INSTALLED_VERSION=""
    fi
}

get_target_version() {
    if [ -n "${VERSION:-}" ]; then
        return
    fi

    local url="https://api.github.com/repos/${REPO}/releases/latest"

    if command -v curl &>/dev/null; then
        VERSION="$(curl -fsSL "$url" | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    elif command -v wget &>/dev/null; then
        VERSION="$(wget -qO- "$url" | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    else
        error "curl or wget is required."
    fi

    [ -n "$VERSION" ] || error "Could not determine latest version. Check https://github.com/${REPO}/releases"
}

download_and_install() {
    local asset="recalld-${PLATFORM}.tar.gz"
    local url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"

    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT

    info "Downloading recalld ${VERSION} for ${PLATFORM}..."

    if command -v curl &>/dev/null; then
        curl -fSL --progress-bar "$url" -o "${TMP_DIR}/${asset}"
    else
        wget -q --show-progress "$url" -O "${TMP_DIR}/${asset}"
    fi

    info "Extracting to ${INSTALL_DIR}..."
    mkdir -p "$INSTALL_DIR"
    tar xzf "${TMP_DIR}/${asset}" -C "$INSTALL_DIR"
    chmod +x "${INSTALL_DIR}/recalld" "${INSTALL_DIR}/recalld-cli"
}

setup_config() {
    local config_dir="$HOME/.recalld"
    local config_file="${config_dir}/config.toml"

    mkdir -p "${config_dir}/data"

    if [ -f "$config_file" ]; then
        return
    fi

    info "Creating default config at ${config_file}..."
    cat > "$config_file" <<'TOML'
# recalld configuration
# Full reference: https://github.com/calebevans/recalld/blob/main/docs/guide.md

[embedding]
provider = "ollama"
model_name = "embeddinggemma:300m"
base_url = "http://localhost:11434"
dimensions = 768

# To use OpenAI instead, uncomment below and set OPENAI_API_KEY:
# provider = "openai"
# model_name = "text-embedding-3-small"
# dimensions = 1536

[decay]
sweep_interval_hours = 24.0
# decay_rate_multiplier = 1.0  # >1.0 = slower decay, <1.0 = faster, 0.0 = disabled

[storage]
# data_dir = "~/.recalld/data"

[server]
# bind_address = "127.0.0.1"
# port = 7680
TOML
}

check_path() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            echo ""
            info "Add ${INSTALL_DIR} to your PATH:"
            echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
            echo ""
            echo "Add that line to your shell profile (~/.bashrc, ~/.zshrc, etc.) to make it permanent."
            ;;
    esac
}

usage() {
    cat <<EOF
Usage: install.sh [OPTIONS]

Install or upgrade recalld.

Options:
  --version <VERSION>   Install a specific version (e.g. v0.2.0)
  --dir <PATH>          Install to a custom directory (default: ~/.local/bin)
  --force               Re-install even if the latest version is already installed
  --help                Show this help message

Environment variables:
  VERSION               Same as --version
  INSTALL_DIR           Same as --dir

Examples:
  curl -fsSL https://raw.githubusercontent.com/calebevans/recalld/main/install.sh | bash
  curl -fsSL ... | bash -s -- --version v0.2.0
  curl -fsSL ... | bash -s -- --force
EOF
    exit 0
}

main() {
    local force=false

    while [ $# -gt 0 ]; do
        case "$1" in
            --version)  VERSION="$2"; shift 2 ;;
            --dir)      INSTALL_DIR="$2"; shift 2 ;;
            --force)    force=true; shift ;;
            --help)     usage ;;
            *)          error "Unknown option: $1. Use --help for usage." ;;
        esac
    done

    detect_platform
    get_installed_version
    get_target_version

    local target_ver="${VERSION#v}"

    if [ -n "$INSTALLED_VERSION" ] && [ "$force" = false ]; then
        if [ "$INSTALLED_VERSION" = "$target_ver" ]; then
            info "recalld ${VERSION} is already installed."
            exit 0
        fi
        info "Upgrading recalld: ${INSTALLED_VERSION} -> ${VERSION}"
    elif [ -n "$INSTALLED_VERSION" ]; then
        info "Reinstalling recalld ${VERSION} (was ${INSTALLED_VERSION})"
    else
        info "Installing recalld ${VERSION}..."
    fi

    download_and_install
    setup_config
    check_path
    echo ""
    info "Done! Run 'recalld --help' to get started."
}

main "$@"
