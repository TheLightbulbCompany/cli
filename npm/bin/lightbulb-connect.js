#!/usr/bin/env node
// Runs the downloaded binary with the same arguments and exit code. If postinstall did not
// run (--ignore-scripts) or vendor/ was removed, downloads it once first.
'use strict';
const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const { install } = require('../scripts/postinstall');

const bin = path.join(__dirname, '..', 'vendor', 'lightbulb');

function run() {
  const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
  if (r.error) throw r.error;
  if (r.signal) process.kill(process.pid, r.signal);
  process.exit(r.status === null ? 1 : r.status);
}

if (fs.existsSync(bin)) {
  run();
} else {
  install().then(run, (err) => {
    console.error(err.plain ? err.message : `Could not install Lightbulb Connect: ${err.message}`);
    process.exit(1);
  });
}
