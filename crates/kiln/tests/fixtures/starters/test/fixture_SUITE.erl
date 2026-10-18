-module(fixture_SUITE).
-include_lib("common_test/include/ct.hrl").
-export([all/0, adds/1]).

all() -> [adds].

adds(_Config) -> 3 = fixture:add(1, 2).
