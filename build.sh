#!/bin/bash
set -e

APP_NAME="SlotShift"
echo "=== Building ${APP_NAME} ==="

# 1. Generate icon
echo "[1/5] Generating icon..."
python3 gen_icon.py

# 2. Compile
echo "[2/5] Compiling (this may take a few minutes)..."
cargo build --release

# 3. Create .app bundle
echo "[3/5] Creating .app bundle..."
rm -rf "${APP_NAME}.app"
mkdir -p "${APP_NAME}.app/Contents/MacOS"
mkdir -p "${APP_NAME}.app/Contents/Resources"

cp target/release/slotshift "${APP_NAME}.app/Contents/MacOS/${APP_NAME}"
cp SlotShift.icns "${APP_NAME}.app/Contents/Resources/"

cat > "${APP_NAME}.app/Contents/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>SlotShift</string>
    <key>CFBundleIdentifier</key>
    <string>com.slotshift.app</string>
    <key>CFBundleName</key>
    <string>SlotShift</string>
    <key>CFBundleIconFile</key>
    <string>SlotShift</string>
    <key>CFBundleVersion</key>
    <string>1.2.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.2.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# 4. Create DMG
echo "[4/5] Packaging DMG..."
rm -rf dmg_tmp
mkdir -p dmg_tmp
cp -R "${APP_NAME}.app" dmg_tmp/
ln -s /Applications dmg_tmp/Applications
rm -f "${APP_NAME}.dmg"
hdiutil create -volname "${APP_NAME}" -srcfolder dmg_tmp -ov -format UDZO "${APP_NAME}.dmg"

# 5. Clean up
echo "[5/5] Cleaning up..."
rm -rf "${APP_NAME}.app" dmg_tmp SlotShift.iconset SlotShift.icns

echo ""
echo "=== Done: ${APP_NAME}.dmg ==="
