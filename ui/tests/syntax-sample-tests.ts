/**
 * The samples' tests ("<lang> highlights as its queries say"), a share of the languages to a test
 * file. Each file is a process of its own: one that ran every language's sample took more memory
 * than CI's ui job has (4 GiB), each grammar's WebAssembly compiled as it first runs, and was killed
 * (syntax.test.ts, SIGKILL, from 2026-11-01).
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { SAMPLES } from "./syntax-samples.ts";
import { classOfWord } from "./syntax-node.ts";

/** How many files the samples are shared among (syntax-samples-1.test.ts and on). */
export const SAMPLE_PARTS = 6;

/** The samples of share `part` (from 0), every SAMPLE_PARTS-th language, each its own test. */
export function sampleTests(part: number) {
  for (const [lang, s] of Object.entries(SAMPLES).filter((_, i) => i % SAMPLE_PARTS === part)) {
    test(`${lang} highlights as its queries say`, async () => {
      for (const [word, cls] of s.expect) assert.equal(await classOfWord(lang, s.code, word), cls, `${lang}: ${JSON.stringify(word)}`);
    });
  }
}
