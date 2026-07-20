'use strict';

// Fetch the prebuilt fin binary from the matching GitHub release and add it to
// this package. Failures here are non-fatal: the launcher (bin/cli.js) retries
// the download on first run, so a transient network error at install time
// doesn't break `npm install`.

const { ensureBinary } = require('../lib/download');

if (process.env.FIN_SKIP_DOWNLOAD) {
  process.stderr.write('fin: FIN_SKIP_DOWNLOAD set, skipping binary download\n');
  process.exit(0);
}

ensureBinary().catch((err) => {
  process.stderr.write(
    `fin: could not download binary during install (${err && err.message ? err.message : err}).\n` +
      `fin: it will be fetched automatically the first time you run \`fin\`.\n`
  );
  process.exit(0);
});
