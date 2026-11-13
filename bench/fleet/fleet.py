#!/usr/bin/env python3
"""Plan step 36's fleet harness (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): microVMs filled to each
host's derived limit, measured, put through chaos and watched for oversubscription, lost or doubled
placement and leaks. README.md says what it does and how to run it."""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from fleetlib import cli  # noqa: E402

if __name__ == "__main__":
    sys.exit(cli.main(sys.argv[1:]))
