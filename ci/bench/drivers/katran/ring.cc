// ci/bench/drivers/katran/ring.cc: VIP 0's Maglev ring, built by Katran's own code (042d08d):
// CHFactory::make(HashFunction::Maglev), Katran's default (KatranLbStructs.h:215), then
// MaglevHash::generateHashRing over N endpoints of weight 1, numbered 1..N (real ids start at 1;
// balancer.bpf.c drops id 0). Katran sets an endpoint's hash to folly::IPAddress::hash() of the
// real's address (KatranLb.cpp:1341); folly isn't built here, so the one shim is that hash: SplitMix64
// of the real's IPv4 address (10.x.y.z), which only seeds Maglev's permutations.
#include <cstdint>
#include <vector>

#include "katran/lib/CHHelpers.h"

static uint64_t lbKatranAddrHash(uint32_t addr) {
  uint64_t z = addr + 0x9e3779b97f4a7c15ULL;
  z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
  z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
  return z ^ (z >> 31);
}

extern "C" int lb_katran_ring_build(uint32_t n, uint32_t ring_size, uint32_t* out) {
  std::vector<katran::Endpoint> endpoints;
  endpoints.reserve(n);
  for (uint32_t i = 0; i < n; i++) {
    katran::Endpoint e;
    e.num = i + 1;
    e.weight = 1;
    e.hash = lbKatranAddrHash((10u << 24) | i);
    endpoints.push_back(e);
  }
  std::vector<int> ring =
      katran::CHFactory::make(katran::HashFunction::Maglev)->generateHashRing(endpoints, ring_size);
  if (ring.size() != ring_size) {
    return -1;
  }
  for (uint32_t i = 0; i < ring_size; i++) {
    if (ring[i] < 1 || static_cast<uint32_t>(ring[i]) > n) {
      return -1;
    }
    out[i] = static_cast<uint32_t>(ring[i]);
  }
  return 0;
}
