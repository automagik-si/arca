# The image an Arca function runs the nightly analysis from. Node with nothing installed: the analysis has no
# dependency. Node 24, the major version CI tests the package on. Pinned by digest (node:24.22.3-alpine, the same index as
# node:24-alpine, read 2026-11-14); a new base is a new digest here and a new function.
FROM docker.io/library/node:24.22.3-alpine@sha256:116b800c3754b440ee0aa3ed2f20f5bf3d99a554bfd9adeea09ea8017f6e85f5
WORKDIR /app
COPY package.json ./
COPY src ./src
USER node
# A function names its command as an absolute path: ["/usr/local/bin/node", "/app/src/cli.js"].
ENTRYPOINT ["/usr/local/bin/node", "/app/src/cli.js"]
