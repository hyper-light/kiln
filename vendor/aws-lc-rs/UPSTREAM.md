# aws-lc-rs, vendored

kiln's cryptography over AWS-LC (rustls's provider, kiln's PKI and signatures), and Mantle's
(`vendor/mantle`), at one copy: the release kiln took from crates.io, with the changes Mantle makes to
it, so Mantle builds on kiln's copy (kiln's CP-T10.2; CP-M1's one revision).

- **Upstream:** https://github.com/aws/aws-lc-rs, tag `v1.18.2`, commit
  `9df497501436596d4da77109217806b3af7f56cb`, ISC and (Apache-2.0 or ISC) (`LICENSE`): the release kiln takes
  (`Cargo.lock`), Mantle's 1.18.1 one release on.
- **What is here:** the crate's folder, `aws-lc-rs`, at upstream's path, and the repository's
  `LICENSE`. Left out: `third_party/NIST/.gitattributes`, by which upstream's checkouts write the NIST
  vectors with CRLF line ends. kiln's copy holds their bytes as upstream's repository does, so a
  checkout of kiln holds what `scripts/vendor.py check` compares; the crate's suite passes on them.
  aws-lc-sys is not here: kiln takes crates.io's 0.46.0, which the crate names by version
  (the change below). Of Mantle's four changes to its 0.45.0, 0.46.0 holds one, AWS-LC's RNDR and
  RDRAND retries (aws/aws-lc#3475); the other three are below.
- **How it's built:** kiln's root `Cargo.toml` points `aws-lc-rs` at `vendor/aws-lc-rs/aws-lc-rs`
  (`[patch.crates-io]`), and so does Mantle's, in kiln's copy of it.

## Changes from upstream

Each change is recorded in `vendor/patches/aws-lc-rs/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `aws-lc-rs/Cargo.toml` | the manifest standalone, as crates.io's package of `1.18.2` states it: its versions and lints resolved, aws-lc-sys and aws-lc-fips-sys by version | the repository's manifest inherits from its workspace and takes them by path from folders kiln doesn't vendor; kiln took the crate from crates.io, and aws-lc-sys stays crates.io's | `0001-its-manifest-standalone-as-crates-io-s-package-states-it.patch` |
| `aws-lc-rs/src/digest.rs`, `src/digest/sha.rs`, `src/digest/state.rs`, `src/hmac.rs`, `src/aead.rs`, `src/aead/tls.rs`, `src/aead/cbc_hmac.rs`, `src/aead/aead_ctx.rs`, `src/aead/unbound_key.rs`, `src/key_wrap.rs`, `src/key_wrap/tests.rs`, `tests/jwe_rfc7516.rs`, `tests/tls13_vectored_seal.rs`, `Cargo.toml` | Mantle's: `digest::MD5_FOR_LEGACY_USE_ONLY`; `Context::try_new`, `try_update` and `try_finish` public; `digest::State`, `Context::state` and `Context::resume`; `hmac::sign_once`; `aead::Tls13VectoredSealingKey`; `aead::cbc_hmac`; `key_wrap::AES_192`; `Clone` for `aead::LessSafeKey`; `autotests = true` | what Mantle builds on (S3's MD5, no panics, a multipart upload's running digest, the one-shot HMAC, TLS 1.3 records from several slices, JWE): Mantle's `vendor/UPSTREAM.md` at `17eb8032` says each, made on 1.18.1; they apply unchanged on 1.18.2 | `0002-mantle-s-changes-md5-fallible-digests-a-digest-s-state-one.patch` |

Mantle's changes are Mantle's to offer upstream; once aws-lc-rs carries one, the re-vendor drops it.

Mantle's other three changes to aws-lc-sys (its `vendor/UPSTREAM.md`) aren't carried as changes:
kiln builds aws-lc-sys 0.46.0 as crates.io's package does. The first two are defaults that
variables set, and kiln's `.cargo/config.toml` sets them as Mantle's builds have them (Ada's word,
2026-11-03); the third waits:

- CPU jitter entropy is built in unless `AWS_LC_SYS_NO_JITTER_ENTROPY=1` (Mantle's: left out unless
  asked for; it costs a process 17.6 ms before its first random bytes): kiln sets it;
- a system AWS-LC is used when one is found (`OPENSSL_DIR`, `OPENSSL_INCLUDE_DIR` and
  `OPENSSL_LIB_DIR`, or pkg-config) unless `AWS_LC_SYS_USE_SYSTEM=0` (Mantle's: only when
  `AWS_LC_SYS_USE_SYSTEM=1` asks): kiln sets it;
- a hardware random-number read that fails all its ten attempts fails the CPU entropy method, and
  AWS-LC aborts the process (Mantle's: read from the operating system instead). That's a change to
  AWS-LC's C, carried only by vendoring aws-lc-sys, which waits until `scripts/vendor.py` takes a
  crates.io package.

## Taking a newer upstream

`python3 scripts/vendor.py aws-lc-rs <commit>` takes the crate at that commit and re-applies the
changes above by three-way merge; take the release kiln's `Cargo.lock` names.
