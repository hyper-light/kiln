"""Django's command-line utility for the fixture's site."""
import os
import sys

from django.core.management import execute_from_command_line


def main():
    """Runs an administrative task."""
    os.environ.setdefault("DJANGO_SETTINGS_MODULE", "fixture_site.settings")
    execute_from_command_line(sys.argv)


if __name__ == "__main__":
    main()
