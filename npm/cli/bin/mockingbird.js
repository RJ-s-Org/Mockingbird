#!/usr/bin/env node
'use strict';

// Runs the mockingbird binary for this platform (see lib/launcher.js).
require('../lib/launcher')
  .main(process.argv.slice(2))
  .then((code) => {
    process.exitCode = code;
  })
  .catch((err) => {
    process.stderr.write(`mockingbird: ${err.stack || err}\n`);
    process.exitCode = 1;
  });
