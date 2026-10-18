"""Tests of the fixture's Python module, for pytest and Django's runner alike."""
import unittest

from fixture import add


class AddTest(unittest.TestCase):
    """add()."""

    def test_adds(self):
        """1 + 2 is 3."""
        self.assertEqual(add(1, 2), 3)
