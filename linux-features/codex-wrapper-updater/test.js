#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const { applyMainBundlePatch, applyWebviewRuntimePatch } = require("./patch.js");
const {
  enabledLinuxFeatureIds,
  loadLinuxFeaturePatchDescriptors,
  stageEnabledLinuxFeatureInstall,
} = require("../../scripts/lib/linux-features.js");

const mainBundle =
  '"use strict";a=require("node:fs");b=require("node:path");c=require("node:os");d=require("node:child_process");var handlers={"native-desktop-apps":async()=>({ok:true})};';

function withTempFeatureConfig(enabled, fn) {
  const originalConfig = process.env.CODEX_LINUX_FEATURES_CONFIG;
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "codex-wrapper-updater-feature-test-"));
  process.env.CODEX_LINUX_FEATURES_CONFIG = path.join(tempDir, "features.json");
  try {
    fs.writeFileSync(process.env.CODEX_LINUX_FEATURES_CONFIG, JSON.stringify({ enabled }, null, 2));
    return fn(path.resolve(__dirname, ".."));
  } finally {
    if (originalConfig == null) {
      delete process.env.CODEX_LINUX_FEATURES_CONFIG;
    } else {
      process.env.CODEX_LINUX_FEATURES_CONFIG = originalConfig;
    }
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
}

test("codex-wrapper-updater stays disabled until listed in features.json", () => {
  withTempFeatureConfig([], (featuresRoot) => {
    assert.deepEqual(enabledLinuxFeatureIds({ featuresRoot }), []);
    assert.deepEqual(loadLinuxFeaturePatchDescriptors({ featuresRoot }), []);
  });
});

test("codex-wrapper-updater exposes descriptors and stages env hook when enabled", () => {
  withTempFeatureConfig(["codex-wrapper-updater"], (featuresRoot) => {
    assert.deepEqual(enabledLinuxFeatureIds({ featuresRoot }), ["codex-wrapper-updater"]);
    assert.deepEqual(
      loadLinuxFeaturePatchDescriptors({ featuresRoot }).map((descriptor) => descriptor.id),
      [
        "feature:codex-wrapper-updater:codex-wrapper-updater-main-handler",
        "feature:codex-wrapper-updater:codex-wrapper-updater-webview-runtime",
      ],
    );

    const appDir = fs.mkdtempSync(path.join(os.tmpdir(), "codex-wrapper-updater-app-"));
    try {
      stageEnabledLinuxFeatureInstall(appDir, { featuresRoot });
      assert.equal(
        fs
          .readFileSync(
            path.join(appDir, ".codex-linux/env.d/codex-wrapper-updater-wrapper-updater.env"),
            "utf8",
          )
          .trim(),
        "CODEX_LINUX_ENABLE_WRAPPER_UPDATES=1",
      );
    } finally {
      fs.rmSync(appDir, { recursive: true, force: true });
    }
  });
});

test("codex-wrapper-updater main patch is idempotent and app-id scoped", () => {
  const patched = applyMainBundlePatch(mainBundle);
  assert.equal(applyMainBundlePatch(patched), patched);
  assert.match(patched, /"codex-linux-wrapper-updater":async/);
  assert.match(patched, /function codexLinuxWrapManagerPath\(\)/);
  assert.match(patched, /function codexLinuxWrapAppId\(\)/);
  assert.match(patched, /join\(d,codexLinuxWrapAppId\(\),`wrapper-update-pending`\)/);
});

test("codex-wrapper-updater webview runtime patch is idempotent", () => {
  const patched = applyWebviewRuntimePatch("const app = true;");
  assert.equal(applyWebviewRuntimePatch(patched), patched);
  assert.match(patched, /codexLinuxWrapperUpdaterVersion/);
  assert.match(patched, /pointer-events:auto/);
  assert.match(patched, /post\(\{action:"check"\}\)/);
});
