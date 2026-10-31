# Semantics: where Amphora's answers depart from SQLite's

Amphora answers as SQLite 3.54.0 answers (the oracles' reference, `bench/oracles`; G2 step 3), except
where a decision here says otherwise. Each one is recorded with its sources, its evidence and what
would falsify it (CLAUDE.md §5). LIKE and GLOB on a BLOB are decided in `planner.md` (decision 3).

## 1. TEXT is well-formed UTF-8

**Decision.** A TEXT value always holds well-formed UTF-8 (`types.rs`, `Text` holds a `str`). Bytes
become text when a BLOB is cast to TEXT or read where text is wanted (`||`, upper(), a JSON
function), and when char() is given a codepoint it can't write (a surrogate, U+D800 to U+DFFF). Each
ill-formed sequence among them is replaced by U+FFFD, substituting maximal subparts (Rust's
`String::from_utf8_lossy`). SQLite keeps the bytes. `CAST(x'ff' AS TEXT)` is the one byte FF there
and the three bytes of U+FFFD here; `char(55296)` is `ED A0 80` there and U+FFFD here.

**Why.** Every text operation in the engine reads a `str`. Holding arbitrary bytes would put a
validity check, or undefined behaviour, behind each one. And SQLite promises nothing for such text,
so none of its answers is the right one to match.

**Sources.** SQLite's "Invalid UTF Policy" (sqlite.org/invalidutf.html, updated 2025-05-31):
"With regard to invalid UTF, SQLite follows a policy of Garbage-In, Garbage-Out (GIGO)"; "If you
insert invalid UTF into an SQLite database, then SQLite makes no guarantees about what text you
might get back out". Its only promise is that invalid UTF causes no memory error and no crash. The
replacement follows the Unicode Standard, chapter 3, section 3.9, "U+FFFD Substitution of Maximal
Subparts".

**Evidence.** The oracles' reference answers hold no ill-formed text: `bench/src/oracle/engines.rs`
skips an answer that does, under SQLite's GIGO policy. Their BLOB literals are well-formed UTF-8
(`synth.rs`). Finding diff-becba9a65574ed81 (`jsonb_array(CAST(x'ff' AS TEXT))`) is filed under
this decision (`bench/oracles/findings.md`). `char_reads_its_integers_whole`
(`crates/engine/tests/oracle_findings.rs`) checks the surrogate.

**Falsified by.** An application that stores ill-formed UTF-8 as TEXT and needs its bytes back. It
should store a BLOB instead.

## 2. atanh's last digit is its platform's C library's

**Decision.** atanh() answers as SQLite's C library does on the platform Amphora is built for
(`vdbe/value.rs`, `cmath::atanh`). On Apple's platforms that is macOS's libm, computed as it
computes it: x itself below 2^-27, above that ln_1p(2|x| / (1 - |x|)) / 2 on the platform's own
log1p, given x's sign. Everywhere else it is the correctly rounded value, glibc 2.44's: computed in
double-double arithmetic (about 106 bits, Dekker's and Knuth's exact sums and products) and
rounded once. Where a platform's C library is neither (an older glibc's fdlibm atanh, a C library
not measured), the last digit can differ from that platform's SQLite.

**Why.** SQLite's atanh is the C library's: `MFUNCTION(atanh, 1, atanh, math1Func)` under
`SQLITE_HAVE_C99_MATH_FUNCS`, and math1Func hands the double straight to it (sqlite3.c 3.54.0, lines
137736 to 137739 and 136780). So its last digit is the platform's: atanh(0.25) is
0.25541281188299536 on glibc 2.44 and 0.25541281188299531 on macOS 26.2, and no one portable
formula gives both. Rust's f64::atanh gives neither: it isn't computed on |x|, so atanh(-x) isn't
-atanh(x), and near -1 it loses half the digits (finding diff-827d8a2d59c98538). The libm crate's
(musl's, after fdlibm) is an ulp off at 0.5, a REAL the generator makes often: 17 findings of step
5's second runs on macOS (`bench/oracles/findings.md`, cause BY).

**Sources.** SQLite 3.54.0's amalgamation (above). T. J. Dekker, "A floating-point technique for
extending the available precision", Numerische Mathematik 18 (1971): FastTwoSum and the exact
product; D. E. Knuth, The Art of Computer Programming, vol. 2, section 4.2.2: TwoSum. The exact
product here comes from one fused multiply-add (`f64::mul_add`, IEEE 754-2008 fusedMultiplyAdd).

**Evidence.** Each computation against the platform's own atanh, called from C, for 3,000,019
arguments (uniform in (-1, 1) and in (-0.2, 0.2), down to 2^-61 binade by binade, 1 - 2^-k for k
up to 52; and ±0, ±1, 2, the thresholds, the smallest subnormal and NaN): the correctly rounded computation
gives glibc 2.44's answer on adacachy for every one, the Apple computation macOS 26.2's on the Mac
Studio for every one, and the correctly rounded value differs from macOS's for 439,712 of them
(14.7%). glibc's equals a 60-digit computation (Python's decimal) for 70,000 random arguments and
for the generator's 159 REALs in (-1, 1), where macOS's differs at 13. The programs and their
output are in `bench/records/2026-10-27-adas-mac-studio-2-cert-clean/atanh/`.
`atanh_has_its_platforms_c_library_digits` and `atanh_is_odd_and_keeps_its_digits_near_minus_one`
(`crates/engine/tests/oracle_findings.rs`) check the answers.

**Falsified by.** An atanh answer of the oracles' differential check differing in its last digit
from SQLite's on the platform it ran on, or the comparison above, rerun, finding a difference: a
macOS whose libm computes atanh otherwise, a glibc whose atanh isn't correctly rounded.
