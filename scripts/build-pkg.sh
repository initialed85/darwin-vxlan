#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/build-pkg.sh [binary_path] [version] [output_dir]
# Example: ./scripts/build-pkg.sh target/release/darwin-vxlan 0.1.0 dist/

BINARY_PATH="${1:-target/release/darwin-vxlan}"
VERSION="${2:-$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')}"
OUTPUT_DIR="${3:-dist}"
IDENTIFIER="com.initialed85.darwin-vxlan"
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

# Generate PackageInfo
cat > "${STAGING_DIR}/PackageInfo" << EOF
<?xml version="1.0" encoding="utf-8"?>
<pkg-info identifier="${IDENTIFIER}" version="${VERSION}"
          install-location="/" auth="root">
    <payload numberOfKilobytes="0"/>
</pkg-info>
EOF

# Generate Distribution.xml
cat > "${STAGING_DIR}/Distribution.xml" << EOF
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>${PACKAGE_NAME}</title>
    <options customize="never" require-scripts="false"/>
    <domains enable_currentUserH="false"/>
    <choices-outline>
        <line choice="choice1"/>
    </choices-outline>
    <choice id="choice1" title="${PACKAGE_NAME}">
        <pkg-ref id="${IDENTIFIER}"/>
    </choice>
    <pkg-ref id="${IDENTIFIER}" version="${VERSION}">${PACKAGE_NAME}-${VERSION}.pkg</pkg-ref>
</installer-gui-script>
EOF

# Calculate installed size in KB
INSTALLED_SIZE=$(du -sk "${STAGING_DIR}/usr" | cut -f1)
sed -i '' "s/numberOfKilobytes=\"0\"/numberOfKilobytes=\"${INSTALLED_SIZE}\"/" "${STAGING_DIR}/PackageInfo"

# Build component package
echo "==> Building component package..."
pkgbuild --root "${STAGING_DIR}" \
         --identifier "${IDENTIFIER}" \
         --version "${VERSION}" \
         --install-location / \
         --ownership recommended \
         "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}-internal.pkg"

# Build product package
echo "==> Building product package..."
productbuild --distribution "${STAGING_DIR}/Distribution.xml" \
             --package-path "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}-internal.pkg" \
             "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg"

# Clean up internal package
rm -f "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}-internal.pkg"

echo "==> Package created: ${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg"
echo "==> Size: $(du -h "${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.pkg" | cut -f1)"
