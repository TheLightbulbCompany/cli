#!/usr/bin/env node
// Downloads the release binary matching this package's version, checks its sha256 and
// puts it at <package>/vendor/lightbulb-connect. No dependencies: https, crypto and the
// system tar (always present on macOS).
'use strict';
const fs = require('fs');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const { execFileSync } = require('child_process');

const REPO = 'TheLightbulbCompany/lightbulb-connect';
const ASSET = 'lightbulb-connect-macos-arm64.tar.gz';

function get(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { 'User-Agent': 'lightbulb-connect-npm' } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && redirects > 0) {
          res.resume();
          return resolve(get(new URL(res.headers.location, url).toString(), redirects - 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url}: HTTP ${res.statusCode}`));
        }
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks)));
        res.on('error', reject);
      })
      .on('error', reject);
  });
}

async function install(root = path.join(__dirname, '..')) {
  if (process.platform !== 'darwin' || process.arch !== 'arm64') {
    throw Object.assign(new Error('Lightbulb Connect needs an Apple-silicon Mac for now.'), { plain: true });
  }
  const { version } = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  const base = `https://github.com/${REPO}/releases/download/v${version}`;
  console.log('Downloading Lightbulb Connect…');
  const [tarball, sums] = await Promise.all([get(`${base}/${ASSET}`), get(`${base}/${ASSET}.sha256`)]);
  const want = sums.toString('utf8').trim().split(/\s+/)[0].toLowerCase();
  const got = crypto.createHash('sha256').update(tarball).digest('hex');
  if (got !== want) {
    throw Object.assign(new Error('Checksum mismatch; not installing.'), { plain: true });
  }
  const vendor = path.join(root, 'vendor');
  fs.mkdirSync(vendor, { recursive: true });
  const tmp = fs.mkdtempSync(path.join(vendor, '.dl-'));
  try {
    const file = path.join(tmp, ASSET);
    fs.writeFileSync(file, tarball);
    execFileSync('tar', ['-xzf', file, '-C', tmp, 'lightbulb-connect']);
    const bin = path.join(vendor, 'lightbulb-connect');
    fs.renameSync(path.join(tmp, 'lightbulb-connect'), bin);
    fs.chmodSync(bin, 0o755);
    return bin;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

module.exports = { install };

if (require.main === module) {
  install(process.argv[2] ? path.resolve(process.argv[2]) : undefined).then(
    () => console.log('Run: lightbulb-connect setup'),
    (err) => {
      console.error(err.plain ? err.message : `Could not install Lightbulb Connect: ${err.message}`);
      process.exit(1);
    }
  );
}
