#!/usr/bin/env node
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { spawn } = require('child_process');

const REPO = 'walkowicz19/klayer';

const ASSET_MAP = {
  'linux-x64': 'klayer-linux-x86_64',
  'win32-x64': 'klayer-windows-x86_64.exe',
  'darwin-x64': 'klayer-macos-x86_64',
  'darwin-arm64': 'klayer-macos-arm64',
};

function fail(message) {
  process.stderr.write(`klayer-mcp: ${message}\n`);
  process.exit(1);
}

function resolveAssetName() {
  const key = `${process.platform}-${process.arch}`;
  const assetName = ASSET_MAP[key];
  if (!assetName) {
    fail(
      `unsupported platform/arch combination "${key}". ` +
        `klayer ships prebuilt binaries only for: ${Object.keys(ASSET_MAP).join(', ')}. ` +
        `Build from source instead: https://github.com/${REPO}`
    );
  }
  return assetName;
}

function getKlayerDir() {
  return path.join(os.homedir(), '.klayer');
}

function httpsGetJson(url, headers) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      url,
      { headers: Object.assign({ 'User-Agent': 'klayer-mcp-npm-launcher' }, headers || {}) },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          resolve(httpsGetJson(res.headers.location, headers));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`GET ${url} failed with status ${res.statusCode}`));
          return;
        }
        let data = '';
        res.setEncoding('utf8');
        res.on('data', (chunk) => (data += chunk));
        res.on('end', () => {
          try {
            resolve(JSON.parse(data));
          } catch (err) {
            reject(new Error(`failed to parse JSON from ${url}: ${err.message}`));
          }
        });
      }
    );
    req.on('error', reject);
  });
}

// GitHub release asset URLs 302-redirect through objects.githubusercontent.com;
// https.get in Node does not follow redirects automatically, so we chase them by hand.
function downloadToFile(url, destPath, redirectsLeft) {
  if (redirectsLeft === undefined) redirectsLeft = 5;
  return new Promise((resolve, reject) => {
    const req = https.get(
      url,
      { headers: { 'User-Agent': 'klayer-mcp-npm-launcher', Accept: 'application/octet-stream' } },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          if (redirectsLeft <= 0) {
            reject(new Error('too many redirects while downloading asset'));
            return;
          }
          resolve(downloadToFile(res.headers.location, destPath, redirectsLeft - 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`download failed with status ${res.statusCode} for ${url}`));
          return;
        }
        const fileStream = fs.createWriteStream(destPath);
        res.pipe(fileStream);
        fileStream.on('finish', () => fileStream.close((err) => (err ? reject(err) : resolve())));
        fileStream.on('error', reject);
      }
    );
    req.on('error', reject);
  });
}

async function resolveReleaseTag() {
  if (process.env.KLAYER_MCP_VERSION) {
    return process.env.KLAYER_MCP_VERSION;
  }
  const release = await httpsGetJson(`https://api.github.com/repos/${REPO}/releases/latest`);
  if (!release || !release.tag_name) {
    throw new Error('GitHub API response did not contain a tag_name');
  }
  return release.tag_name;
}

async function resolveAssetUrl(tag, assetName) {
  if (process.env.KLAYER_MCP_VERSION) {
    return `https://github.com/${REPO}/releases/download/${tag}/${assetName}`;
  }
  const release = await httpsGetJson(`https://api.github.com/repos/${REPO}/releases/latest`);
  const asset = (release.assets || []).find((a) => a.name === assetName);
  if (!asset) {
    throw new Error(`release ${release.tag_name} has no asset named "${assetName}"`);
  }
  return asset.browser_download_url;
}

async function ensureBinary(assetName) {
  const tag = await resolveReleaseTag();
  const cacheDir = path.join(getKlayerDir(), 'bin', tag);
  const destPath = path.join(cacheDir, assetName);

  if (fs.existsSync(destPath)) {
    return destPath;
  }

  fs.mkdirSync(cacheDir, { recursive: true });
  const url = await resolveAssetUrl(tag, assetName);

  // Download to a temp path first and rename into place only once the full
  // file is written, so a failed/interrupted download never leaves a
  // corrupt binary sitting at the cache path for the next invocation to pick up.
  const tmpPath = `${destPath}.download-${process.pid}`;
  try {
    await downloadToFile(url, tmpPath);
    fs.renameSync(tmpPath, destPath);
  } catch (err) {
    try {
      fs.unlinkSync(tmpPath);
    } catch (_) {
      // best-effort cleanup
    }
    throw err;
  }

  if (process.platform !== 'win32') {
    fs.chmodSync(destPath, 0o755);
  }

  return destPath;
}

function launch(binaryPath, args) {
  const child = spawn(binaryPath, args, { stdio: 'inherit' });
  child.on('error', (err) => {
    fail(`failed to launch ${binaryPath}: ${err.message}`);
  });
  child.on('exit', (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

async function main() {
  const assetName = resolveAssetName();
  const args = process.argv.slice(2);

  let binaryPath;
  try {
    binaryPath = await ensureBinary(assetName);
  } catch (err) {
    fail(`could not obtain klayer binary: ${err.message}`);
    return;
  }

  launch(binaryPath, args);
}

main();
