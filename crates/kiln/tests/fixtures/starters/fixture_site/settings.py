"""Settings of the fixture's Django site: no database, no apps."""

SECRET_KEY = "fixture-only"
DEBUG = True
INSTALLED_APPS = []
DATABASES = {}
USE_TZ = True
