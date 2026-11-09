// kiln's own .kiln/release.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "release",
  "on": {
    "push": {
      "branches": [
        "main"
      ]
    },
    "tag": {
      "branches": [
        "v*"
      ]
    },
    "manual": {}
  },
  "concurrency": {
    "group": "release-${{ event }}-${{ branch }}",
    "cancel-in-progress": false
  },
  "jobs": {
    "linux": {
      "name": "Linux (x86-64, ARM64)",
      "runs-on": [
        "baremetal"
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
          "name": "aarch64 musl",
          "run": "nice -n 10 ci/release/build.sh aarch64-unknown-linux-musl"
        }
      ],
      "artifacts": [
        "dist/*"
      ]
    },
    "windows": {
      "name": "Windows (x86-64, cross-built)",
      "runs-on": [
        "baremetal"
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
          "name": "build",
          "run": "nice -n 10 ci/release/build.sh x86_64-pc-windows-msvc"
        }
      ],
      "artifacts": [
        "dist/*"
      ]
    },
    "macos": {
      "name": "macOS (ARM64), on a Mac",
      "runs-on": [
        "baremetal",
        "macos"
      ],
      "requires": [
        "os == macos",
        "arch == aarch64",
        "tools has cargo",
        "tools has node"
      ],
      "timeout-minutes": 90,
      "steps": [
        {
          "name": "ui",
          "run": "export PATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\" && npm ci --prefix ui --no-audit --no-fund && KILN_UI_SERVER=off npm run --prefix ui build"
        },
        {
          "name": "build",
          "run": "nice -n 10 ci/release/build.sh aarch64-apple-darwin"
        }
      ],
      "artifacts": [
        "dist/*"
      ]
    }
  }
});
