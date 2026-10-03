import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import test from "node:test";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "../../..");
const run = (...args) => execFileSync("node", ["packages/mobile-design/scripts/generate.mjs", ...args], { cwd: root, encoding: "utf8" });

test("generated mobile design resources are deterministic", () => {
  run();
  run("--check");
});

test("generated resource syntax has required platform roots", () => {
  assert.match(readFileSync(resolve(root, "apps/android/app/src/main/res/values/strings_peppy.xml"), "utf8"), /^<\?xml[\s\S]*<resources>/);
  assert.match(readFileSync(resolve(root, "apps/android/app/src/main/res/drawable/peppy_hero.xml"), "utf8"), /<vector /);
  const iosStrings = JSON.parse(readFileSync(resolve(root, "apps/ios/PeppyMobile/Peppy.xcstrings"), "utf8"));
  assert.equal(iosStrings.sourceLanguage, "en");
  assert.equal(iosStrings.version, "1.0");
  assert.match(readFileSync(resolve(root, "apps/ios/PeppyMobile/Design/PeppyTokens.swift"), "utf8"), /struct PeppyColorScheme/);
});
