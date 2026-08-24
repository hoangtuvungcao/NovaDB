#!/usr/bin/env bash
# ==============================================================================
# NovaDB Universal Installer (Linux & macOS)
# ==============================================================================
# Usage:
#   curl -fsSL https://get.novadb.io | bash
#   or
#   ./scripts/install.sh
# ==============================================================================

set -euo pipefail

BOLD="$(tput bold 2>/dev/null || echo '')"
GREEN="$(tput setaf 2 2>/dev/null || echo '')"
CYAN="$(tput setaf 6 2>/dev/null || echo '')"
YELLOW="$(tput setaf 3 2>/dev/null || echo '')"
RED="$(tput setaf 1 2>/dev/null || echo '')"
RESET="$(tput sgr0 2>/dev/null || echo '')"

info() {
    echo "${CYAN}${BOLD}==>${RESET} ${BOLD}$*${RESET}"
}

success() {
    echo "${GREEN}${BOLD}✓${RESET} $*"
}

warn() {
    echo "${YELLOW}${BOLD}!${RESET} $*"
}

error() {
    echo "${RED}${BOLD}✗${RESET} $*" >&2
    exit 1
}

# 1. Detect OS and Architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

case "$OS" in
    linux)
        TARGET_OS="unknown-linux-gnu"
        ;;
    darwin)
        TARGET_OS="apple-darwin"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"
INSTALL_DIR="${NOVADB_INSTALL_DIR:-/usr/local/bin}"
DATA_DIR="${NOVADB_DATA_DIR:-/var/lib/novadb}"

info "Installing NovaDB for ${TARGET}..."

# 2. Check permissions for system directory
USE_SUDO=""
if [ ! -w "$INSTALL_DIR" ] && [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        USE_SUDO="sudo"
    else
        INSTALL_DIR="${HOME}/.local/bin"
        mkdir -p "$INSTALL_DIR"
    fi
fi

# 3. Build from local source if available, or fetch binary
if [ -f "./Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    info "Detected local source directory. Building release binaries with cargo..."
    cargo build --release -p novadb-cli -p novadb-server
    $USE_SUDO install -m 755 target/release/novadb "${INSTALL_DIR}/novadb"
    $USE_SUDO install -m 755 target/release/novadbd "${INSTALL_DIR}/novadbd"
else
    # In production releases, this pulls prebuilt tarballs from GitHub Releases
    RELEASE_URL="https://github.com/novadb/novadb/releases/latest/download/novadb-${TARGET}.tar.gz"
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT

    info "Downloading NovaDB from ${RELEASE_URL}..."
    if curl -fsSL "$RELEASE_URL" -o "${TMP_DIR}/novadb.tar.gz" 2>/dev/null; then
        tar -xzf "${TMP_DIR}/novadb.tar.gz" -C "$TMP_DIR"
        $USE_SUDO install -m 755 "${TMP_DIR}/novadb" "${INSTALL_DIR}/novadb"
        $USE_SUDO install -m 755 "${TMP_DIR}/novadbd" "${INSTALL_DIR}/novadbd"
    else
        warn "Pre-built binary not found on remote. Building from source via cargo..."
        if command -v cargo >/dev/null 2>&1; then
            cargo install --git https://github.com/novadb/novadb --bin novadb --bin novadbd --root "${INSTALL_DIR}/.."
        else
            error "Rust toolchain (cargo) is required to build NovaDB. Install rustup: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        fi
    fi
fi

# 4. Create default data directory
if [ "$(id -u)" -eq 0 ] || [ -n "$USE_SUDO" ]; then
    $USE_SUDO mkdir -p "$DATA_DIR"
fi

success "NovaDB binaries successfully installed to ${INSTALL_DIR}/novadb and ${INSTALL_DIR}/novadbd"

# 5. Setup systemd service on Linux if systemd exists
if [ "$OS" = "linux" ] && command -v systemctl >/dev/null 2>&1 && [ -d "/etc/systemd/system" ]; then
    SERVICE_FILE="/etc/systemd/system/novadb.service"
    info "Setting up systemd service at ${SERVICE_FILE}..."
    cat <<EOF | $USE_SUDO tee "$SERVICE_FILE" >/dev/null
[Unit]
Description=NovaDB Server and PostgreSQL Wire Protocol Gateway
Documentation=https://novadb.dev
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=${DATA_DIR}
ExecStart=${INSTALL_DIR}/novadbd --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir ${DATA_DIR}
Restart=always
RestartSec=3
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
    $USE_SUDO systemctl daemon-reload || true
    success "Systemd service installed. Start with: sudo systemctl enable --now novadb"
fi

echo ""
echo "${GREEN}${BOLD}================================================================${RESET}"
echo "${GREEN}${BOLD} 🎉 NovaDB is installed and ready!${RESET}"
echo "${GREEN}${BOLD}================================================================${RESET}"
echo ""
echo "  ${BOLD}Quick Start:${RESET}"
echo "    novadb init myapp.novadb                    # Create a database"
echo "    novadb query myapp.novadb -s \"SELECT 1+1\"    # Run SQL queries"
echo "    novadb serve --pg-listen 127.0.0.1:5432     # Start PostgreSQL server"
echo ""
echo "  ${BOLD}Connect via PostgreSQL tools:${RESET}"
echo "    psql -h 127.0.0.1 -p 5432 -d default"
echo ""
echo "  ${BOLD}Documentation:${RESET}"
echo "    Visit https://novadb.dev or see /docs/ in repository."
echo ""
