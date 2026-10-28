#!/usr/bin/env bash
# dupRemove 发布管线：门禁 → 构建 → 重签 → DMG →（有证书则）公证 → 验证。
#
# 签名策略：
#   - 无 Developer ID → ad-hoc 重签（可挂 GitHub Release；用户需右键打开或 xattr）
#   - 有 Developer ID → Hardened Runtime + 可选公证
#
# 公证凭证（二选一，没有则跳过公证并提示）：
#   export NOTARY_PROFILE=xxx            # xcrun notarytool store 的 keychain profile
#   export APPLE_ID=... APPLE_PASSWORD=... APPLE_TEAM_ID=...
#
# 用法：npm run release

set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="dupRemove"
BUNDLE_DIR="src-tauri/target/release/bundle"
APP="$BUNDLE_DIR/macos/$APP_NAME.app"

say() { printf "\033[1;34m==> %s\033[0m\n" "$*"; }
fail() { printf "\033[1;31m✗ %s\033[0m\n" "$*" >&2; exit 1; }

# ---- 1. 质量门禁：fmt / clippy（-D warnings）/ test / 前端构建 ----
say "质量门禁：check"
npm run check

# ---- 2. 构建产物 ----
say "构建 release（tauri build）"
npm run tauri build 2>&1 | tail -4
[ -d "$APP" ] || fail "未产出 $APP"

# ---- 3. 重新签名 ----
# tauri build 自带的 ad-hoc 签名过不了 codesign --strict（resources 清单不一致），
# 必须整体重签。发布用的 Developer ID + Hardened Runtime 也在这里统一处理。
IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
  | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)".*/\1/' || true)
NOTARIZED=0
if [ -n "$IDENTITY" ]; then
  say "Developer ID 签名：$IDENTITY（Hardened Runtime）"
  codesign --force --deep --options runtime --sign "$IDENTITY" "$APP"
  SIGN_MODE="developer-id"
else
  say "未找到 Developer ID 证书 —— ad-hoc 重签"
  codesign --force --deep --sign - "$APP"
  SIGN_MODE="adhoc"
fi

# ---- 4. 自打 DMG ----
# .app 重签后 Tauri 生成的 DMG 里是旧副本，必须重打；
# 用 hdiutil 直打，不依赖 bundle_dmg.sh 的 Finder 自动化（曾报 -10006）。
say "打包 DMG（hdiutil）"
DMG="$BUNDLE_DIR/dmg/${APP_NAME}_$(git rev-parse --short HEAD)_aarch64.dmg"
rm -f "$DMG"
STAGING=$(mktemp -d)
trap 'rm -rf "$STAGING"' EXIT
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG" >/dev/null

# ---- 5. 公证（仅 Developer ID 模式）----
if [ "$SIGN_MODE" = "developer-id" ]; then
  codesign --sign "$IDENTITY" "$DMG"
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    say "公证（notarytool profile：$NOTARY_PROFILE）"
    xcrun notarytool submit "$DMG" --profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
    NOTARIZED=1
  elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    say "公证（APPLE_ID 凭证）"
    xcrun notarytool submit "$DMG" \
      --apple-id "$APPLE_ID" --password "${APPLE_PASSWORD:-}" --team-id "$APPLE_TEAM_ID" --wait
    xcrun stapler staple "$DMG"
    NOTARIZED=1
  else
    say "⚠ 未配置公证凭证（NOTARY_PROFILE 或 APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID），已跳过公证"
  fi
fi

# ---- 6. 验证 ----
say "验证产物"
codesign --verify --deep --strict "$APP" || fail "codesign --strict 校验失败"
echo "  ✓ codesign --verify --deep --strict"
hdiutil verify "$DMG" >/dev/null || fail "DMG 校验失败"
echo "  ✓ hdiutil verify"
if [ "$SIGN_MODE" = "developer-id" ]; then
  spctl --assess --type execute "$APP" || fail "Gatekeeper 评估失败"
  echo "  ✓ spctl --assess（Gatekeeper）"
  if [ "$NOTARIZED" = "1" ]; then
    echo "  ✓ 已公证并 staple"
  fi
else
  echo "  ✓ ad-hoc：codesign 校验通过（Gatekeeper：右键打开或 xattr -cr）"
fi

# ---- 7. 产物清单 ----
say "产物"
du -sh "$APP" "$DMG"
echo "  · $APP"
echo "  · $DMG"
if [ "$SIGN_MODE" = "developer-id" ] && [ "$NOTARIZED" = "1" ]; then
  echo "  ✅ Developer ID + 公证（Gatekeeper 友好，可选升级路径）"
elif [ "$SIGN_MODE" = "adhoc" ]; then
  echo "  ✅ DMG 就绪（ad-hoc，可挂 Release）"
else
  echo "  ⚠ 已 Developer ID 签名但未公证 —— 配置 NOTARY_PROFILE 或 APPLE_ID"
fi
