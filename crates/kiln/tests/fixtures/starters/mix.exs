defmodule Fixture.MixProject do
  use Mix.Project

  def project do
    [app: :fixture, version: "0.1.0", elixir: "~> 1.14", elixirc_paths: ["ex"], test_paths: ["ex_test"], deps: []]
  end

  def application, do: [extra_applications: [:logger]]
end
