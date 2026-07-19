"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const { pipeline } = require("node:stream/promises");
const { Readable } = require("node:stream");
const packageJson = require("../package.json");

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

async function sha256(file) {
  const hash = crypto.createHash("sha256");
  await pipeline(fs.createReadStream(file), hash);
  return hash.digest("hex");
}

async function download(url, destination) {
  const response = await fetch(url, {
    redirect: "follow"
  });
  if (!response.ok || !response.body) {
    throw new Error(`download failed (${response.status}) for ${url}`);
  }
  await pipeline(Readable.fromWeb(response.body), fs.createWriteStream(destination, { mode: 0o755 }));
}

async function install() {
  const target = targetName();
  if (!target) {
    throw new Error(`unsupported platform ${process.platform}/${process.arch}`);
  }

  const nativeDir = path.join(__dirname, "..", "native");
  const destination = path.join(nativeDir, target);
  const partial = `${destination}.partial`;
  await fs.promises.mkdir(nativeDir, { recursive: true });
  await fs.promises.rm(partial, { force: true });

  const localBinary = process.env.POLYTREAD_BINARY_PATH;
  if (localBinary) {
    await fs.promises.copyFile(path.resolve(localBinary), partial);
  } else {
    const base = (process.env.POLYTREAD_RELEASE_BASE_URL ||
      `https://github.com/EH-a0/polytread/releases/download/v${packageJson.version}`).replace(/\/$/, "");
    const checksumResponse = await fetch(`${base}/${target}.sha256`, {
      redirect: "follow"
    });
    if (!checksumResponse.ok) {
      throw new Error(`checksum download failed (${checksumResponse.status})`);
    }
    const expected = (await checksumResponse.text()).trim().split(/\s+/)[0].toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(expected)) {
      throw new Error("release checksum file is invalid");
    }
    await download(`${base}/${target}`, partial);
    const actual = await sha256(partial);
    if (actual !== expected) {
      throw new Error(`binary checksum mismatch (expected ${expected}, received ${actual})`);
    }
  }

  await fs.promises.chmod(partial, 0o755);
  await fs.promises.rm(destination, { force: true });
  await fs.promises.rename(partial, destination);
  console.log(`Installed PolyTread native binary for ${process.platform}/${process.arch}.`);
}

install().catch(async error => {
  console.error(`PolyTread installation failed: ${error.message}`);
  process.exitCode = 1;
});
