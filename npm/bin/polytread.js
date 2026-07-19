#!/usr/bin/env node

"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

function targetName() {
  const key = `${process.platform}-${process.arch}`;
  const targets = {
    "win32-x64": "polytread-windows-x64.exe",
    "linux-x64": "polytread-linux-x64",
    "linux-arm64": "polytread-linux-arm64",
    "darwin-x64": "polytread-macos-x64",
    "darwin-arm64": "polytread-macos-arm64"
  };
  return targets[key];
}

const target = targetName();
if (!target) {
  console.error(`PolyTread does not provide a binary for ${process.platform}/${process.arch}.`);
  process.exit(1);
}

const binary = path.join(__dirname, "..", "native", target);
if (!fs.existsSync(binary)) {
  console.error("The PolyTread native binary is missing. Reinstall the NPM package to repair it.");
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: false
});

child.on("error", error => {
  console.error(`Unable to start PolyTread: ${error.message}`);
  process.exitCode = 1;
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}

child.on("exit", (code, signal) => {
  if (signal && process.platform !== "win32") {
    process.kill(process.pid, signal);
    return;
  }
  process.exitCode = code === null ? 1 : code;
});
