// kiln's own .kiln/image.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "image",
  "on": {
    "push": {
      "branches": [
        "main"
      ]
    },
    "manual": {}
  },
  "concurrency": {
    "group": "image",
    "cancel-in-progress": false
  },
  "jobs": {
    "image": {
      "name": "build and publish kiln's image",
      "runs-on": [
        "baremetal",
        "local"
      ],
      "requires": [
        "os == linux",
        "tools has cargo",
        "tools has python3",
        "tools has node"
      ],
      "timeout-minutes": 90,
      "steps": [
        {
          "name": "ui",
          "run": "export PATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\" && npm ci --prefix ui --no-audit --no-fund && KILN_UI_SERVER=off npm run --prefix ui build"
        },
        {
          "name": "x86_64 musl",
          "run": "KILN_COMMIT=$KILN_SHA nice -n 10 ci/release/build.sh x86_64-unknown-linux-musl"
        },
        {
          "name": "build",
          "run": "if readelf -l dist/kiln-x86_64-unknown-linux-musl | grep -q INTERP; then echo \"dist/kiln-x86_64-unknown-linux-musl is not static\" >&2; exit 1; fi\ndocker build -q --target prebuilt --build-arg KILN_BINARY=dist/kiln-x86_64-unknown-linux-musl --label kiln.published=kiln -t kiln:$KILN_SHA .\n"
        },
        {
          "name": "publish",
          "run": "if [ -n \"${KILN_IMAGE_KILN:-}\" ]; then\n  docker tag \"kiln:$KILN_SHA\" \"$KILN_IMAGE_KILN:$KILN_SHA\"\n  docker push \"$KILN_IMAGE_KILN:$KILN_SHA\"\nelse\n  bash deploy/cluster/load-image.sh \"kiln:$KILN_SHA\"\nfi\n"
        },
        {
          "name": "prune",
          "run": "bash deploy/cluster/prune-images.sh kiln 20"
        }
      ]
    }
  }
});
