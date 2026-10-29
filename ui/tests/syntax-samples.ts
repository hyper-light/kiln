/**
 * A piece of code in every language kiln highlights, and what some of its words must look like:
 * a keyword, a string, a comment, a function, a type, a property, and what's written in it in
 * other languages (a `<style>`'s CSS, a fence's Rust).
 */
export const SAMPLES: Record<string, { code: string; expect: [word: string, cls: string][] }> = {
  astro: {
    code: `---
const title = "Hi";
---
<h1 class="big">{title}</h1>
<style>h1 { color: red; }</style>`,
    expect: [
      ["const", "hl-kw"],
      ['"Hi"', "hl-str"],
      ["h1", "hl-tag"],
      ["color", "hl-prop"],
    ],
  },
  bash: {
    code: `#!/usr/bin/env bash
# deploy it
name="kiln"
if [ -n "$name" ]; then
  echo "hello" | grep -c 1
fi`,
    expect: [
      ["# deploy it", "hl-com"],
      ["if", "hl-kw"],
      ["echo", "hl-fn"],
      ['"hello"', "hl-str"],
    ],
  },
  c: {
    code: `#include <stdio.h>
/* sum */
static int add(int a, int b) { return a + b; }
int main(void) { printf("%d", add(1, 2)); return MAX_SIZE; }`,
    expect: [
      ["/* sum */", "hl-com"],
      ["static", "hl-kw"],
      ["add", "hl-fn"],
      ['"%d"', "hl-str"],
      ["1", "hl-num"],
      ["MAX_SIZE", "hl-num"],
    ],
  },
  cmake: {
    code: `cmake_minimum_required(VERSION 3.20)
# kiln
project(kiln LANGUAGES C)
if(WIN32)
  set(SOURCES "main.c")
endif()`,
    expect: [
      ["cmake_minimum_required", "hl-fn"],
      ["# kiln", "hl-com"],
      ["if", "hl-kw"],
      ['"main.c"', "hl-str"],
    ],
  },
  cpp: {
    code: `#include <vector>
namespace kiln {
template <typename T> class Box { public: T value; };
auto make() -> std::vector<int> { return {1, 2}; }
}`,
    expect: [
      ["namespace", "hl-kw"],
      ["class", "hl-kw"],
      ["Box", "hl-type"],
      ["make", "hl-fn"],
      ["value", "hl-prop"],
    ],
  },
  csharp: {
    code: `using System;
namespace Kiln {
  public class Job {
    public string Name { get; set; } = "build";
    public int Run(int n) => n * 2;
  }
}`,
    expect: [
      ["using", "hl-kw"],
      ["Job", "hl-type"],
      ['"build"', "hl-str"],
      ["Run", "hl-fn"],
      ["2", "hl-num"],
    ],
  },
  css: {
    code: `/* card */
.card > a:hover { color: #fff; margin: 4px 0; }
@media (max-width: 600px) { .card { display: none; } }`,
    expect: [
      ["/* card */", "hl-com"],
      ["color", "hl-prop"],
      ["@media", "hl-kw"],
    ],
  },
  dart: {
    code: `import 'dart:io';
// jobs
class Job { final String name; Job(this.name); int run() => 1; }
void main() { print(Job('x').run()); }`,
    expect: [
      ["import", "hl-kw"],
      ["// jobs", "hl-com"],
      ["'x'", "hl-str"],
      ["1", "hl-num"],
    ],
  },
  dockerfile: {
    code: `FROM node:24-alpine AS ui
# build the UI
RUN npm ci && npm run build
ENV NODE_ENV=production`,
    expect: [
      ["FROM", "hl-kw"],
      ["# build the UI", "hl-com"],
      ["npm", "hl-fn"],
    ],
  },
  elixir: {
    code: `defmodule Kiln.Job do
  # jobs
  def run(name, opts \\\\ []) do
    IO.puts("running")
    {:ok, name}
  end
end`,
    expect: [
      ["defmodule", "hl-kw"],
      ["# jobs", "hl-com"],
      ["run", "hl-fn"],
      ['"running"', "hl-str"],
      [":ok", "hl-num"],
    ],
  },
  erlang: {
    code: `-module(kiln).
%% jobs
run(Name) -> io:format("~s", [Name]), ok.`,
    expect: [
      ["%% jobs", "hl-com"],
      ["run", "hl-fn"],
    ],
  },
  go: {
    code: `package main
import "fmt"
// a job
type Job struct { Name string }
func (j *Job) Run(n int) error { fmt.Println(j.Name, n, 42); return nil }`,
    expect: [
      ["package", "hl-kw"],
      ['"fmt"', "hl-str"],
      ["// a job", "hl-com"],
      ["Job", "hl-type"],
      ["Name", "hl-prop"],
      ["Run", "hl-fn"],
      ["Println", "hl-fn"],
      ["42", "hl-num"],
    ],
  },
  graphql: {
    code: `# jobs
query GetJob($id: ID!) {
  job(id: $id) { name status @include(if: true) }
}
type Job implements Node { id: ID! name: String }`,
    expect: [
      ["# jobs", "hl-com"],
      ["query", "hl-kw"],
      ["GetJob", "hl-fn"],
      ["ID", "hl-type"],
      ["name", "hl-prop"],
      ["true", "hl-num"],
    ],
  },
  haskell: {
    code: `module Main where
import Data.List (sort)
-- entry
main :: IO ()
main = putStrLn (show (sum [1, 2, 3]))`,
    expect: [
      ["module", "hl-kw"],
      ["-- entry", "hl-com"],
      ["IO", "hl-type"],
      ["1", "hl-num"],
    ],
  },
  hcl: {
    code: `# web
resource "aws_instance" "web" {
  ami           = var.ami
  instance_type = "t3.micro"
  count         = 2
}`,
    expect: [
      ["# web", "hl-com"],
      ["resource", "hl-type"],
      ["ami", "hl-prop"],
      ["var", "hl-kw"],
      ["2", "hl-num"],
    ],
  },
  html: {
    code: `<!DOCTYPE html>
<html lang="en">
<head><style>body { margin: 0; }</style></head>
<body><p class="x">Hi</p><script>const n = 1;</script></body>
</html>`,
    expect: [
      ["head", "hl-tag"],
      ["en", "hl-str"],
      ["margin", "hl-prop"],
      ["const", "hl-kw"],
    ],
  },
  ini: {
    code: `; config
[server]
port = 7070`,
    expect: [
      ["; config", "hl-com"],
      ["port", "hl-prop"],
    ],
  },
  java: {
    code: `package kiln;
import java.util.List;
public class Job { private final String name;
  @Override public String toString() { return "Job " + name + 42; } }`,
    expect: [
      ["package", "hl-kw"],
      ["Job", "hl-type"],
      ['"Job "', "hl-str"],
      ["toString", "hl-fn"],
      ["42", "hl-num"],
    ],
  },
  javascript: {
    code: `import { run } from "./run.js";
// begin
export const MAX = 3;
/** @returns {number} */
function build(opts = {}) { return run(opts.name, 42, null); }
class Job extends Base { start() { console.log(this); } }
const el = <Card title="x" />;`,
    expect: [
      ["import", "hl-kw"],
      ['"./run.js"', "hl-str"],
      ["// begin", "hl-com"],
      ["MAX", "hl-num"],
      ["@returns", "hl-kw"],
      ["build", "hl-fn"],
      ["42", "hl-num"],
      ["Job", "hl-type"],
      ["start", "hl-fn"],
      ["Card", "hl-type"],
    ],
  },
  jsdoc: {
    code: `/** @param {string} name the job */`,
    expect: [["@param", "hl-kw"]],
  },
  json: {
    code: `{ "name": "kiln", "count": 3, "ok": true, "none": null }`,
    expect: [
      ['"name"', "hl-prop"],
      ['"kiln"', "hl-str"],
      ["3", "hl-num"],
      ["true", "hl-num"],
    ],
  },
  julia: {
    code: `module Kiln
function run(n::Int)
    x = sum([1, 2, n])  # total
    return x * 2.5
end
end`,
    expect: [
      ["module", "hl-kw"],
      ["function", "hl-kw"],
      ["# total", "hl-com"],
      ["2.5", "hl-num"],
    ],
  },
  kotlin: {
    code: `package kiln
// jobs
data class Job(val name: String) { fun run(n: Int): Int = n * 42 }
fun main() { println(Job("x").run(1)) }`,
    expect: [
      ["package", "hl-kw"],
      ["// jobs", "hl-com"],
      ["Job", "hl-type"],
      ['"x"', "hl-str"],
      ["42", "hl-num"],
    ],
  },
  lua: {
    code: `-- config
local M = {}
function M.run(name, n)
  if n > 0 then print("job " .. name) end
  return nil
end`,
    expect: [
      ["-- config", "hl-com"],
      ["local", "hl-kw"],
      ["print", "hl-fn"],
      ['"job "', "hl-str"],
      ["0", "hl-num"],
    ],
  },
  make: {
    code: `CC ?= cc
# build
all: kiln
.PHONY: all`,
    expect: [["# build", "hl-com"]],
  },
  markdown: {
    code: `# Title

Some *em* and \`code\` and a [link](https://x.y).

\`\`\`rust
fn main() {}
\`\`\``,
    expect: [
      ["Title", "hl-strong"],
      ["fn", "hl-kw"],
      ["main", "hl-fn"],
    ],
  },
  markdown_inline: {
    code: "Some *em* and `code`",
    expect: [
      ["em", "hl-em"],
      ["code", "hl-str"],
    ],
  },
  nix: {
    code: `{ pkgs ? import <nixpkgs> {} }:
# shell
let name = "kiln"; in
pkgs.mkShell { buildInputs = [ pkgs.rustc ]; }`,
    expect: [
      ["# shell", "hl-com"],
      ["let", "hl-kw"],
      ['"kiln"', "hl-str"],
    ],
  },
  objc: {
    code: `#import <Foundation/Foundation.h>
// a job
@interface Job : NSObject
- (void)run:(int)n;
@end`,
    expect: [
      ["// a job", "hl-com"],
      ["@interface", "hl-kw"],
    ],
  },
  ocaml: {
    code: `(* jobs *)
let rec fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)
let () = print_endline "kiln"`,
    expect: [
      ["(* jobs *)", "hl-com"],
      ["let", "hl-kw"],
      ["fib", "hl-fn"],
      ['"kiln"', "hl-str"],
      ["2", "hl-num"],
    ],
  },
  ocaml_interface: {
    code: `(* the job *)
val fib : int -> int`,
    expect: [
      ["(* the job *)", "hl-com"],
      ["val", "hl-kw"],
    ],
  },
  php: {
    code: `<?php
namespace Kiln;
// jobs
function run(string $name): int { echo "job"; return 42; }
?>
<p class="x">Hi</p>`,
    expect: [
      ["function", "hl-kw"],
      ["// jobs", "hl-com"],
      ["run", "hl-fn"],
      ['"job"', "hl-str"],
      ["42", "hl-num"],
      ["p", "hl-tag"],
    ],
  },
  php_only: {
    code: `$n = strlen("kiln"); // count`,
    expect: [
      ["strlen", "hl-fn"],
      ['"kiln"', "hl-str"],
    ],
  },
  perl: {
    code: `use strict;
# jobs
my $name = "kiln";
sub run { my ($n) = @_; return $n * 2; }`,
    expect: [
      ["use", "hl-kw"],
      ["# jobs", "hl-com"],
      ["my", "hl-kw"],
      ['"kiln"', "hl-str"],
      ["run", "hl-fn"],
      ["2", "hl-num"],
    ],
  },
  powershell: {
    code: `# jobs
function Get-Job {
  param([string]$Name)
  Write-Output "job"
}
$count = 3`,
    expect: [
      ["# jobs", "hl-com"],
      ["function", "hl-kw"],
      ['"job"', "hl-str"],
    ],
  },
  proto: {
    code: `syntax = "proto3";
// jobs
message Job { string name = 1; repeated int32 ids = 2; }`,
    expect: [
      ["syntax", "hl-meta"],
      ["// jobs", "hl-com"],
      ["message", "hl-kw"],
      ["1", "hl-num"],
    ],
  },
  python: {
    code: `import os
# go
class Job(Base):
    def run(self, n: int = 3) -> str:
        return "job" if n else None
print(Job().run(), True, 1.5)`,
    expect: [
      ["import", "hl-kw"],
      ["# go", "hl-com"],
      ["Job", "hl-type"],
      ["run", "hl-fn"],
      ['"job"', "hl-str"],
      ["None", "hl-num"],
      ["print", "hl-fn"],
      ["1.5", "hl-num"],
    ],
  },
  r: {
    code: `library(dplyr)
# summarize
run <- function(x, n = 2) { mean(x) * n }`,
    expect: [
      ["# summarize", "hl-com"],
      ["function", "hl-kw"],
      ["2", "hl-num"],
    ],
  },
  ruby: {
    code: `require "json"
# a job
class Job < Base
  def run(n = 1)
    puts("job", :ok, nil)
  end
end`,
    expect: [
      ["# a job", "hl-com"],
      ["class", "hl-kw"],
      ["Job", "hl-type"],
      ["run", "hl-fn"],
      ['"job"', "hl-str"],
      [":ok", "hl-num"],
    ],
  },
  rust: {
    code: `use std::fmt;
/// A job.
#[derive(Debug)]
pub struct Job { name: String }
impl Job { pub fn run(&self, n: u32) -> Option<u32> { println!("{}", self.name); Some(n * 2) } }
const MAX: usize = 42;`,
    expect: [
      ["use", "hl-kw"],
      ["/// A job.", "hl-com"],
      ["Job", "hl-type"],
      ["name", "hl-prop"],
      ["run", "hl-fn"],
      ['"{}"', "hl-str"],
      ["MAX", "hl-num"],
      ["42", "hl-num"],
    ],
  },
  scala: {
    code: `package kiln
// jobs
object Main extends App {
  def run(n: Int): Int = n * 2
  println("x")
}`,
    expect: [
      ["package", "hl-kw"],
      ["// jobs", "hl-com"],
      ["run", "hl-fn"],
      ["Int", "hl-type"],
      ['"x"', "hl-str"],
    ],
  },
  solidity: {
    code: `pragma solidity ^0.8.0;
// kiln
contract Kiln { uint256 public count = 1; function run(address to) public returns (bool) { return true; } }`,
    expect: [
      ["// kiln", "hl-com"],
      ["contract", "hl-kw"],
      ["run", "hl-fn"],
      ["1", "hl-num"],
    ],
  },
  sql: {
    code: `-- jobs
SELECT name, COUNT(*) AS n FROM jobs WHERE status = 'done' LIMIT 10;`,
    expect: [
      ["-- jobs", "hl-com"],
      ["SELECT", "hl-kw"],
      ["'done'", "hl-str"],
      ["10", "hl-num"],
    ],
  },
  svelte: {
    code: `<script lang="ts">
  let count: number = 0;
</script>
<button on:click={() => count++}>Clicks: {count}</button>
{#if count > 2}<p>many</p>{/if}
<style>button { color: red; }</style>`,
    expect: [
      ["let", "hl-kw"],
      ["button", "hl-tag"],
      ["color", "hl-prop"],
    ],
  },
  swift: {
    code: `import Foundation
// A job
struct Job { let name: String; func run(n: Int) -> Int { return n * 2 } }
print(Job(name: "x").run(n: 3))`,
    expect: [
      ["import", "hl-kw"],
      ["// A job", "hl-com"],
      ["run", "hl-fn"],
      ['"x"', "hl-str"],
      ["2", "hl-num"],
    ],
  },
  toml: {
    code: `# config
[package]
name = "kiln"
edition = 2024
features = ["derive"]`,
    expect: [
      ["# config", "hl-com"],
      ["name", "hl-prop"],
      ['"kiln"', "hl-str"],
      ["2024", "hl-num"],
    ],
  },
  tsx: {
    code: `import * as React from "react";
type Props = { title: string };
export function Card({ title }: Props) { return <div className="card">{title}</div>; }`,
    expect: [
      ["import", "hl-kw"],
      ['"react"', "hl-str"],
      ["Props", "hl-type"],
      ["Card", "hl-type"],
      ["div", "hl-tag"],
    ],
  },
  typescript: {
    code: `import type { Job } from "./api";
// runs
interface Run { id: number; job?: Job }
export async function start(run: Run): Promise<void> { const n: number = 42; }`,
    expect: [
      ["import", "hl-kw"],
      ["// runs", "hl-com"],
      ["Run", "hl-type"],
      ["number", "hl-type"],
      ["start", "hl-fn"],
      ["42", "hl-num"],
    ],
  },
  vue: {
    code: `<template>
  <div class="app">{{ msg }}</div>
</template>
<script setup lang="ts">
const msg: string = "hi";
</script>
<style>.app { color: red; }</style>`,
    expect: [
      ["div", "hl-tag"],
      ["const", "hl-kw"],
      ['"hi"', "hl-str"],
      ["color", "hl-prop"],
    ],
  },
  xml: {
    code: `<?xml version="1.0"?>
<!-- jobs -->
<jobs><job name="build">make</job></jobs>`,
    expect: [
      ["<!-- jobs -->", "hl-com"],
      ["job", "hl-tag"],
      ["build", "hl-str"],
    ],
  },
  yaml: {
    code: `# kiln
name: ci
jobs:
  build:
    retries: 2
    enabled: true
    steps:
      - run: cargo test`,
    expect: [
      ["# kiln", "hl-com"],
      ["name", "hl-prop"],
      ["ci", "hl-str"],
      ["2", "hl-num"],
      ["true", "hl-num"],
    ],
  },
  zig: {
    code: `const std = @import("std");
// entry
pub fn main() !void { const n: u32 = 42; std.debug.print("{d}", .{n}); }`,
    expect: [
      ["const", "hl-kw"],
      ["// entry", "hl-com"],
      ["main", "hl-fn"],
      ["42", "hl-num"],
      ['"{d}"', "hl-str"],
    ],
  },
};
