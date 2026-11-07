# Profile checks, 2026-11-04

`kiln-lab browser check-profiles --out ~/.cache/kiln-bp/g1/checks/seed1 --seed 1` on the Linux lab
machine (kiln-a094a12d, Fedora 44, kernel 6.19.10, tc 6.17.0 from Fedora's own signed iproute-tc
package unpacked under ~/.cache), uid 1000, no sudo. Exit 0.

| profile | throughput (% of rate) | median RTT (ms) against base | off | full queue (ms) against set | reordered |
| --- | --- | --- | --- | --- | --- |
| good | 100.0 | 20.1 against 20 (1000 pings) | 0.5% | - | 0 |
| 100k | 99.9 | 818.3 against 800 (1000 pings) | 2.3% | 3912 against 4000 | 0 |
| 50k | 99.7 | 827.4 against 800 (1000 pings) | 3.4% | 3873 against 4000 | 0 |

| loss | way | seed | lost of 100,000 | 99% band | same indices as seed's first run | shared with the first seed's | runs again (lab-side drops) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1% | down | 1 | 975 | 918 to 1082 | - | - | 0 |
| 1% | down | 1 | 975 | 918 to 1082 | yes | - | 0 |
| 1% | down | 6364136223846793006 | 1070 | 918 to 1082 | - | 9 | 0 |
| 1% | up | 1 | 982 | 918 to 1082 | - | - | 0 |
| 1% | up | 1 | 982 | 918 to 1082 | yes | - | 0 |
| 1% | up | 6364136223846793006 | 989 | 918 to 1082 | - | 6 | 0 |
| 5% | down | 1 | 4939 | 4822 to 5178 | - | - | 0 |
| 5% | down | 1 | 4939 | 4822 to 5178 | yes | - | 0 |
| 5% | down | 6364136223846793006 | 5031 | 4822 to 5178 | - | 277 | 0 |
| 5% | up | 1 | 5024 | 4822 to 5178 | - | - | 0 |
| 5% | up | 1 | 5024 | 4822 to 5178 | yes | - | 0 |
| 5% | up | 6364136223846793006 | 4942 | 4822 to 5178 | - | 238 | 0 |

Every check passed.

The RTT runs a little over the base: a 58-byte ping frame takes 9.3 ms each way at 50 kbit/s (4.6 ms
at 100), and netem's FIFO lets no packet leave before the one ahead of it, which lifts the median
of a jittered delay. Another seed shares about as many lost indices with the first as chance would
(1% of 1,000 and 5% of 5,000). A first run at 20,000 packets a second on a loaded machine lost
packets in the lab's own receiver (its socket's buffer), not on the link: the check now reads its
namespace's `Udp: RcvbufErrors`, runs a run with any again (up to three times, each counted), and
sends at 5,000 a second. A second full check (seed 2) on the same machine while it ran at a load of
about 400 kept dropping in the receiver and stopped, saying so, as it should.
