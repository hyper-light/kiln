# kiln's bench package in Envoy's tree (ci/bench/build-envoy.sh copies it to
# test/extensions/load_balancing_policies/kiln_bench): Envoy's selection kernels under the common
# trace. Not Envoy's code; the libraries it links are.
load(
    "//bazel:envoy_build_system.bzl",
    "envoy_cc_test_binary",
    "envoy_package",
)

licenses(["notice"])  # Apache 2

envoy_package()

envoy_cc_test_binary(
    name = "envoy_pick",
    srcs = [
        "common.h",
        "envoy_pick.cc",
    ],
    deps = [
        "//source/extensions/load_balancing_policies/least_request:least_request_lb_lib",
        "//source/extensions/load_balancing_policies/random:random_lb_lib",
        "//source/extensions/load_balancing_policies/round_robin:round_robin_lb_lib",
        "//test/extensions/load_balancing_policies/common:benchmark_base_tester_lib",
        "@abseil-cpp//absl/container:flat_hash_map",
    ],
)
