# GitHub's expression test vectors (docs/ci.md W01-AC01)

`testdata/*.json` are copied verbatim from
[`actions/languageservices`](https://github.com/actions/languageservices), `expressions/testdata`,
at commit `4043eda158e16579cc5fb1b0b07a4bce2a76f0b5` (2026-08-05), under its MIT licence (`LICENSE`).
They are the cross-language tests GitHub keeps for its expression implementations; `../oracle.rs`
runs every one of them, including the five GitHub's TypeScript implementation skips (kiln follows
the C# runner there). Refresh with `./refresh.sh`.
