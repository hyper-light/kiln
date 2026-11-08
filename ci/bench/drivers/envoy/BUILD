# kiln's bench package in Envoy's tree: run.py copies it, with ci/bench/common.h, to
# test/extensions/load_balancing_policies/kiln_bench and builds it with bazel -c opt.
# Envoy's load balancers, set up as its own *_lb_benchmark.cc set them up (BaseTester), under the
# bench's workload. Not Envoy's code; the libraries it links are.
load(
    "//bazel:envoy_build_system.bzl",
    "envoy_cc_test_binary",
    "envoy_package",
)

licenses(["notice"])  # Apache 2

envoy_package()

envoy_cc_test_binary(
    name = "envoy_driver",
    srcs = [
        "common.h",
        "envoy.cc",
    ],
    deps = [
        "//source/common/upstream:host_utility_lib",
        "//source/extensions/load_balancing_policies/least_request:least_request_lb_lib",
        "//source/extensions/load_balancing_policies/maglev:maglev_lb_lib",
        "//source/extensions/load_balancing_policies/random:random_lb_lib",
        "//source/extensions/load_balancing_policies/ring_hash:ring_hash_lb_lib",
        "//source/extensions/load_balancing_policies/round_robin:round_robin_lb_lib",
        "//test/extensions/load_balancing_policies/common:benchmark_base_tester_lib",
        "@abseil-cpp//absl/container:flat_hash_map",
    ],
)
