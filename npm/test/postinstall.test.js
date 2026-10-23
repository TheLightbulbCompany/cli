// Runs postinstall against the real v0.2.1 release in a temp dir (needs network) and checks
// the extracted binary reports its version.
'use strict';
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const VERSION = '0.2.1';
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'lbc-npm-'));
try {
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version: VERSION }));
  const out = execFileSync(process.execPath, [path.join(__dirname, '..', 'scripts', 'postinstall.js'), root], {
    encoding: 'utf8',
  });
  assert.match(out, /Run: lightbulb-connect setup/);
  const bin = path.join(root, 'vendor', 'lightbulb-connect');
  assert.strictEqual(fs.statSync(bin).mode & 0o777, 0o755);
  const version = execFileSync(bin, ['version'], { encoding: 'utf8' }).trim();
  assert.strictEqual(version, `lightbulb-connect ${VERSION}`);
  console.log('ok:', version);
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
