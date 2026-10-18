require "spec"
require "../src/fixture"

describe Fixture do
  it "adds" do
    Fixture.add(1, 2).should eq 3
  end
end
