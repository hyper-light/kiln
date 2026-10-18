require "minitest/autorun"
require "fixture"

class FixtureTest < Minitest::Test
  def test_adds
    assert_equal 3, Fixture.add(1, 2)
  end
end
