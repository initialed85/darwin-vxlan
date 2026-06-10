#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/build-pkg.sh [binary_path] [version] [output_dir]
# Example: ./scripts/build-pkg.sh target/release/darwin-vxlan 0.1.0 dist/

BINARY_PATH="${1:-target/release/darwin-vxlan}"
VERSION="${2:-$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')}"
OUTPUT_DIR="${3:-dist}"
IDENTIFIER="com.cyborgside.darwin-vxlan"
PACKAGE_NAME="darwin-vxlan"

echo "==> Building .pkg for ${PACKAGE_NAME} v${VERSION}"

# Verify binary exists and is ARM64
if [ ! -f "${BINARY_PATH}" ]; then
    echo "ERROR: Binary not found at ${BINARY_PATH}"
    echo "Run 'cargo build --release' first."
    exit 1
fi

ARCH=$(file "${BINARY_PATH}" | grep -o "arm64\|x86_64")
if [ "${ARCH}" != "arm64" ]; then
    echo "WARNING: Binary architecture is ${ARCH}, expected arm64"
fi

# Create staging directory
STAGING_DIR=$(mktemp -d)
trap 'rm -rf "${STAGING_DIR}"' EXIT

# Copy binary to staging
mkdir -p "${STAGING_DIR}/usr/local/bin"
cp "${BINARY_PATH}" "${STAGING_DIR}/usr/local/bin/${PACKAGE_NAME}"
chmod 755 "${STAGING_DIR}/usr/local/bin/${PACKAGE_NAME}"

# Create output directory
mkdir -p "${OUTPUT_DIR}"

# Build package
echo "==> Building package..."
pkgbuild --root "${STAGING_DIR}" \
         --identifier "${IDENTIFIER}" \
         --version "${VERSION}" \
         --install-location / \
         --ownership recommended \
         "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg"

echo "==> Package created: ${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg"
echo "==> Size: $(du -h "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg" | cut -f1)"
