import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import { catalog, desktopCatalog, desktopOnlyKeys } from "../src/catalog.mjs";

const root = resolve(import.meta.dirname, "../../..");
const run = (...args) => execFileSync("node", ["packages/mobile-design/scripts/generate.mjs", ...args], { cwd: root, encoding: "utf8" });
const path = file => resolve(root, file);

function luminance(hex) {
  const expanded = hex.length === 3 ? [...hex].map(value => value + value).join("") : hex;
  const channels = expanded.match(/\w\w/g).map(value => parseInt(value, 16) / 255).map(value => value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}
function contrast(first, second) {
  const [light, dark] = [luminance(first), luminance(second)].sort((left, right) => right - left);
  return (light + 0.05) / (dark + 0.05);
}
function pngDimensions(file) {
  const png = readFileSync(path(file));
  assert.deepEqual([...png.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);
  return [png.readUInt32BE(16), png.readUInt32BE(20)];
}

function themeTokens(css, selector, start = 0) {
  const selectorStart = css.indexOf(selector, start);
  assert.notEqual(selectorStart, -1, `missing ${selector}`);
  const body = css.slice(selectorStart, css.indexOf("}", selectorStart));
  return Object.fromEntries([...body.matchAll(/--peppy-([\w-]+):\s*(#[0-9a-f]{3,6});/gi)].map(([, key, value]) => [key, value.slice(1)]));
}

test("generated mobile design resources are deterministic", () => {
  run();
  run("--check");
});

test("CSS and generated tokens retain coral action foreground contrast", () => {
  const css = readFileSync(path("packages/desktop-ui/src/peppy-tokens.css"), "utf8");
  const dark = themeTokens(css, ":root,");
  const light = themeTokens(css, ".theme-light");
  const systemLight = themeTokens(css, ".theme-system", css.indexOf("@media (prefers-color-scheme: light)"));

  for (const [name, theme] of Object.entries({ dark, light, systemLight })) {
    for (const [background, foreground] of [["accent", "accent-text"], ["accent-hover", "accent-text"], ["primary", "primary-text"]]) {
      // The approved #C9422D / white light pair rounds to 4.5:1 but has a
      // precise sRGB ratio just below that threshold.
      assert.ok(contrast(theme[background], theme[foreground]) >= 4.4, `${name} ${foreground} must contrast with ${background}`);
    }
  }

  assert.equal(light["accent-text"], "fff");
  assert.equal(light["primary-text"], "fff");
  assert.equal(systemLight["accent-text"], "fff");
  assert.equal(systemLight["primary-text"], "fff");
  assert.equal(dark["accent-text"], "1a1a1a");
  assert.equal(dark["primary-text"], "1a1a1a");

  const android = readFileSync(path("apps/android/app/src/main/java/dev/peppy/mobile/ui/theme/PeppyTokens.kt"), "utf8");
  const swift = readFileSync(path("apps/ios/PeppyMobile/Design/PeppyTokens.swift"), "utf8");
  assert.match(android, /val Light = PeppyColorScheme\([\s\S]*accentText = Color\(0xFFFFFFFF\),[\s\S]*primaryText = Color\(0xFFFFFFFF\)/);
  assert.match(swift, /static let light = PeppyColorScheme\([\s\S]*AccentText: Color\(argb: 0xFFFFFFFF\),[\s\S]*PrimaryText: Color\(argb: 0xFFFFFFFF\)/);
});

test("android resources preserve canonical mark geometry within the safe zone", () => {
  const mark = readFileSync(path("packages/mobile-design/assets/peppy-mark.svg"), "utf8");
  const sourcePath = mark.match(/\bd="\s*([\s\S]*?)"\s*\/>/)?.[1].replace(/\s+/g, " ").trim();
  const foreground = readFileSync(path("apps/android/app/src/main/res/drawable/ic_launcher_foreground.xml"), "utf8");
  const logo = readFileSync(path("apps/android/app/src/main/res/drawable/peppy_logo.xml"), "utf8");
  assert.equal(sourcePath, foreground.match(/android:pathData="([^"]+)"/)?.[1]);
  assert.equal(sourcePath, logo.match(/android:pathData="([^"]+)"/)?.[1]);
  assert.match(logo, /android:scaleY="0.92"/);
  const attribute = name => Number(foreground.match(new RegExp(`android:${name}="([0-9.]+)"`))[1]);
  const sx = attribute("scaleX"), sy = attribute("scaleY");
  const tx = attribute("translateX"), ty = attribute("translateY");
  assert.equal(sy / sx, 0.92, "canonical aspect must be retained");
  // Sample this canonical M/H/V/L/C path's curves, not merely the bounding rectangle.
  const tokens = sourcePath.match(/[A-Z]|-?\d+(?:\.\d+)?/g);
  let i = 0, x = 0, y = 0;
  const number = () => Number(tokens[i++]);
  const point = (px, py) => assert.ok(Math.hypot(tx + px * sx - 54, ty + py * sy - 54) <= 33, "visible mark stays in the circular safe zone");
  while (i < tokens.length) {
    const command = tokens[i++];
    if (command === "M" || command === "L") { x = number(); y = number(); point(x, y); }
    else if (command === "H") { x = number(); point(x, y); }
    else if (command === "V") { y = number(); point(x, y); }
    else if (command === "C") {
      const x1 = number(), y1 = number(), x2 = number(), y2 = number(), x3 = number(), y3 = number();
      for (let sample = 0; sample <= 100; sample++) {
        const t = sample / 100, u = 1 - t;
        point(u ** 3 * x + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t ** 3 * x3,
          u ** 3 * y + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t ** 3 * y3);
      }
      x = x3; y = y3;
    } else assert.equal(command, "Z");
  }
});

test("generated copy has both native catalog formats", () => {
  assert.match(readFileSync(path("apps/android/app/src/main/res/values/strings_peppy.xml"), "utf8"), /peppy_onboarding_headline/);
  const strings = JSON.parse(readFileSync(path("apps/ios/PeppyMobile/Peppy.xcstrings"), "utf8"));
  assert.equal(strings.strings["peppy.locked"].localizations.en.stringUnit.value, "Locked");
});

test("account resolution copy maps to each generated platform format", () => {
  const copy = {
    hosted_account_checking: "Checking your account…",
    hosted_account_check_failed: "We couldn’t check your account. Try again.",
    hosted_data_preparing: "Preparing your data…",
    hosted_data_prepare_failed: "Preparation couldn’t finish. Try again.",
  };
  const android = readFileSync(path("apps/android/app/src/main/res/values/strings_peppy.xml"), "utf8");
  const ios = JSON.parse(readFileSync(path("apps/ios/PeppyMobile/Peppy.xcstrings"), "utf8"));
  const desktop = readFileSync(path("apps/desktop/src/generated/peppyCopy.ts"), "utf8");

  for (const [key, value] of Object.entries(copy)) {
    assert.equal(catalog[key], value);
    assert.match(android, new RegExp(`<string name="peppy_${key}">${value}</string>`));
    assert.equal(ios.strings[`peppy.${key}`].localizations.en.stringUnit.value, value);
    assert.ok(desktop.includes(`${key}: ${JSON.stringify(value)},`));
  }
});

test("generated desktop copy applies the desktop catalog overrides", () => {
  const desktop = readFileSync(path("apps/desktop/src/generated/peppyCopy.ts"), "utf8");
  assert.match(desktop, /^\/\/ Generated by packages\/mobile-design\/scripts\/generate\.mjs\. Do not edit\.\n/);
  assert.match(desktop, /export type PeppyCopyKey = keyof typeof peppyCopy;/);

  for (const [key, value] of Object.entries(desktopCatalog)) {
    assert.ok(desktop.includes(`${key}: ${JSON.stringify(value)},`), `Key ${key} with its desktop value should be in generated copy`);
  }
});

test("desktop override keys are catalog keys or explicit desktop-only additions", () => {
  const desktopOverrideKeys = [
    "hosted_join_headline", "hosted_join_body", "device_approval_headline", "device_approval_body",
    "hosted_unlock_body", "hosted_unlock_cta", "hosted_subscribe_legal", "hosted_subscribe_restore",
    "hosted_subscribe_store_unavailable", "hosted_purchase_pending_body", "passphrase_create_body",
    "passphrase_native_cta", "passphrase_native_note", "passphrase_ack_label", "permissions_notifications",
    "permissions_login", "permissions_desktop_note", "self_hosted_body", "hosted_preview_native_only",
    "passphrase_weak",
  ];

  for (const key of desktopOverrideKeys) {
    assert.ok(key in catalog || desktopOnlyKeys.includes(key), `${key} must be shared or explicitly desktop-only`);
    assert.ok(key in desktopCatalog, `${key} must be present in desktop copy`);
  }

  for (const key of desktopOnlyKeys) {
    assert.ok(!(key in catalog), `${key} must be absent from shared catalog`);
  }
});

test("native copy is generated from the shared catalog only", () => {
  const android = readFileSync(path("apps/android/app/src/main/res/values/strings_peppy.xml"), "utf8");
  const ios = readFileSync(path("apps/ios/PeppyMobile/Peppy.xcstrings"), "utf8");
  assert.doesNotMatch(android, /Add this computer/);
  assert.doesNotMatch(ios, /Add this computer/);
  assert.match(android, new RegExp(catalog.hosted_join_headline));
  assert.match(ios, new RegExp(catalog.hosted_join_headline));
});

test("generated icons include default, dark, and tinted variants", () => {
  assert.deepEqual(pngDimensions("apps/desktop/src-tauri/icons/icon.png"), [512, 512]);
  for (const file of ["AppIcon-512@2x.png", "AppIcon-512@2x-dark.png", "AppIcon-512@2x-tinted.png"]) {
    assert.deepEqual(pngDimensions(`apps/ios/PeppyMobile/Assets.xcassets/AppIcon.appiconset/${file}`), [1024, 1024]);
    assert.ok(statSync(path(`apps/ios/PeppyMobile/Assets.xcassets/AppIcon.appiconset/${file}`)).size > 500);
  }
  const contents = JSON.parse(readFileSync(path("apps/ios/PeppyMobile/Assets.xcassets/AppIcon.appiconset/Contents.json"), "utf8"));
  const variants = contents.images.filter(image => image.appearances);
  assert.equal(contents.images.length, 3, "one universal 1024px source per appearance");
  assert.ok(contents.images.every(image => image.idiom === "universal" && image.platform === "ios" && image.size === "1024x1024"));
  assert.equal(variants.length, 2, "dark and tinted appearances must be registered");
  assert.deepEqual([...new Set(variants.map(image => image.appearances[0].value))].sort(), ["dark", "tinted"]);
});

test("desktop logo and favicon are direct generated copies of the canonical mark", () => {
  const mark = readFileSync(path("packages/mobile-design/assets/peppy-mark.svg"), "utf8");
  assert.equal(readFileSync(path("apps/desktop/public/peppy-logo.svg"), "utf8"), mark);
  assert.equal(readFileSync(path("apps/desktop/public/favicon.svg"), "utf8"), mark);
});
