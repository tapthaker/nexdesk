#!/bin/sh
# Nexdesk Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/tapthaker/nexdesk/master/install.sh | sh

set -e

# Parse arguments
NON_INTERACTIVE=false
for arg in "$@"; do
  case "$arg" in
    --non-interactive) NON_INTERACTIVE=true ;;
  esac
done

# Colors (disabled in non-interactive mode or non-tty)
if [ "$NON_INTERACTIVE" = true ] || [ ! -t 1 ]; then
  RED='' GREEN='' YELLOW='' BLUE='' DIM='' NC=''
else
  RED=$(printf '\033[0;31m')
  GREEN=$(printf '\033[0;32m')
  YELLOW=$(printf '\033[1;33m')
  BLUE=$(printf '\033[0;34m')
  DIM=$(printf '\033[2m')
  NC=$(printf '\033[0m')
fi

printf '\n%snexdesk%s - Cross-platform KVM sharing\n\n' "$BLUE" "$NC"

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)
    case "$ARCH" in
      x86_64)        PLATFORM="linux-x86_64" ;;
      *) printf '%sUnsupported architecture: %s%s\n' "$RED" "$ARCH" "$NC"; exit 1 ;;
    esac
    ;;
  darwin)
    case "$ARCH" in
      x86_64) PLATFORM="macos-x86_64" ;;
      arm64)  PLATFORM="macos-aarch64" ;;
      *) printf '%sUnsupported architecture: %s%s\n' "$RED" "$ARCH" "$NC"; exit 1 ;;
    esac
    ;;
  *)
    printf '%sUnsupported OS: %s%s\n' "$RED" "$OS" "$NC"
    echo "Download manually: https://github.com/tapthaker/nexdesk/releases"
    exit 1
    ;;
esac

# Resolve the latest release download URL from GitHub
REPO="tapthaker/nexdesk"
LATEST_URL="https://github.com/$REPO/releases/latest/download/nexdesk-$PLATFORM"

# Set paths
INSTALL_DIR="$HOME/.local/bin"
BINARY="$INSTALL_DIR/nexdesk"

# Create directory
mkdir -p "$INSTALL_DIR"

# Download to temp file first, then atomic move (handles "Text file busy" when upgrading)
TMPFILE="$INSTALL_DIR/.nexdesk.tmp.$$"
printf 'Downloading nexdesk-%s...\n' "$PLATFORM"
if command -v curl > /dev/null 2>&1; then
  curl -fsSL "$LATEST_URL" -o "$TMPFILE"
elif command -v wget > /dev/null 2>&1; then
  wget -q "$LATEST_URL" -O "$TMPFILE"
else
  printf '%scurl or wget required%s\n' "$RED" "$NC"
  exit 1
fi

chmod +x "$TMPFILE"
mv -f "$TMPFILE" "$BINARY"
printf '%sDownloaded%s %s-> %s%s\n' "$GREEN" "$NC" "$DIM" "$BINARY" "$NC"

# ============================================
# PATH Configuration
# ============================================

PATH_EXPORT='export PATH="$HOME/.local/bin:$PATH"'

# Detect shell config file
detect_shell_config() {
  case "$SHELL" in
    */zsh)
      if [ -f "$HOME/.zshrc" ]; then
        echo "$HOME/.zshrc"
      elif [ -f "$HOME/.zprofile" ]; then
        echo "$HOME/.zprofile"
      else
        echo "$HOME/.zshrc"
      fi
      ;;
    */bash)
      if [ -f "$HOME/.bashrc" ]; then
        echo "$HOME/.bashrc"
      elif [ -f "$HOME/.bash_profile" ]; then
        echo "$HOME/.bash_profile"
      elif [ -f "$HOME/.profile" ]; then
        echo "$HOME/.profile"
      else
        echo "$HOME/.bashrc"
      fi
      ;;
    */fish)
      echo "$HOME/.config/fish/config.fish"
      ;;
    *)
      if [ -f "$HOME/.profile" ]; then
        echo "$HOME/.profile"
      else
        echo "$HOME/.bashrc"
      fi
      ;;
  esac
}

# Check if PATH already has .local/bin in config
is_path_configured() {
  config_file="$1"
  [ -f "$config_file" ] && grep -q '\.local/bin' "$config_file" 2>/dev/null
}

# Check if already in current PATH
in_path() {
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) return 0 ;;
    *) return 1 ;;
  esac
}

# Add PATH to config file
configure_path() {
  config_file="$1"

  # Fish shell uses different syntax
  if [ "$config_file" = "$HOME/.config/fish/config.fish" ]; then
    mkdir -p "$HOME/.config/fish"
    echo "" >> "$config_file"
    echo "# Nexdesk" >> "$config_file"
    echo 'set -gx PATH $HOME/.local/bin $PATH' >> "$config_file"
  else
    echo "" >> "$config_file"
    echo "# Nexdesk" >> "$config_file"
    echo "$PATH_EXPORT" >> "$config_file"
  fi
}

# Main PATH setup
SHELL_CONFIG=$(detect_shell_config)

if in_path; then
  printf '\n%sInstalled!%s\n' "$GREEN" "$NC"

elif is_path_configured "$SHELL_CONFIG"; then
  printf '\n%sInstalled!%s\n' "$GREEN" "$NC"
  printf '\nRestart your shell or run:\n'
  printf '\n  %ssource %s%s\n' "$BLUE" "$SHELL_CONFIG" "$NC"

elif [ "$NON_INTERACTIVE" = true ]; then
  printf '\n%sInstalled!%s\n' "$GREEN" "$NC"
  printf '\nAdd to your shell config:\n'
  printf '\n  %s%s%s\n' "$BLUE" "$PATH_EXPORT" "$NC"

else
  printf '\n%s~/.local/bin is not in your PATH%s\n\n' "$YELLOW" "$NC"
  printf "Add it to %s? [Y/n] " "$SHELL_CONFIG"

  # Read from /dev/tty if stdin is a pipe
  if [ -t 0 ]; then
    read -r response
  else
    read -r response < /dev/tty 2>/dev/null || response="y"
  fi

  case "$response" in
    [nN]|[nN][oO])
      printf '\n%sInstalled!%s\n' "$GREEN" "$NC"
      printf '\nAdd to your shell config:\n'
      printf '\n  %s%s%s\n' "$BLUE" "$PATH_EXPORT" "$NC"
      ;;
    *)
      configure_path "$SHELL_CONFIG"
      printf '\n%sInstalled!%s %sUpdated %s%s\n' "$GREEN" "$NC" "$DIM" "$SHELL_CONFIG" "$NC"
      printf '\nRestart your shell or run:\n'
      printf '\n  %ssource %s%s\n' "$BLUE" "$SHELL_CONFIG" "$NC"
      ;;
  esac
fi

# ============================================
# Run interactive setup wizard
# ============================================

if [ "$NON_INTERACTIVE" != true ] && [ -t 1 ]; then
  printf '\n'
  # Ensure the binary is in PATH for this session
  export PATH="$INSTALL_DIR:$PATH"
  # The binary handles reopening /dev/tty internally when stdin is a pipe,
  # so no shell-level redirect is needed (and it breaks macOS kqueue).
  nexdesk setup
fi
