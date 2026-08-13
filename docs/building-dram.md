# Building a Homebrew-compatible package manager in a day, with an AI

*by [spoodyz](https://github.com/spoodyz). Disclosure, and honestly the point
of this post: most of dram's code was written by Claude (Anthropic's Fable
model) working in my terminal over one day, with me steering, testing, and
breaking things. This write-up was drafted by the same model and edited by
me. TWiR asks for LLM authorship to be disclosed, and I think that's a good
rule.*

[dram](https://github.com/spoodyz/dram) is a bottle-only, Homebrew-compatible
package manager for macOS, written in Rust. It installs the same packages
from the same registry brew uses, about 2x faster on cold installs and 8x
faster on metadata operations, and it adds per-project locked environments.
It's 3,231 lines of Rust across 8 commits, all from August 13th. This post
covers the parts that were technically interesting, the bugs real packages
exposed, and what the human/AI split actually looked like.

## The premise

Homebrew's slowness is mostly its architecture: a Ruby interpreter, formula
DSL evaluation, and git-based taps run on every invocation. But the actual
package data is two public endpoints. `formulae.brew.sh/api/formula.json` is
every formula's metadata, dependencies, and bottle URLs in one JSON dump, and
the bottles themselves are plain OCI blobs on `ghcr.io` that anyone can pull
anonymously. So a client can be: fetch JSON, resolve the dependency DAG,
download blobs, extract, fix up paths. No Ruby anywhere.

The hard parts are all in "fix up paths."

## Relocation, or why binaries kept getting killed

Bottles are compiled against `/opt/homebrew`, and Homebrew rewrites paths at
bottling time into placeholders like `@@HOMEBREW_PREFIX@@`, including inside
Mach-O load commands (`LC_LOAD_DYLIB`, `LC_RPATH`, `LC_ID_DYLIB`). dram
installs to `~/.dram`, so it has to rewrite every placeholder to real paths:
`otool -l` to find them, `install_name_tool` to rewrite them.

The part that bites: editing a Mach-O invalidates its code signature, and on
Apple silicon an invalid signature isn't a warning, it's SIGKILL on exec. So
every patched binary gets re-ad-hoc-signed with `codesign -f -s -`
immediately after rewriting. Miss one and the binary just dies with no useful
error.

One design decision here turned out to matter more than expected. Instead of
rewriting `@@HOMEBREW_PREFIX@@/opt/<dep>/lib/...` to point at a mutable
`opt/` symlink the way brew does, dram rewrites it to the exact versioned keg
path, `Cellar/<dep>/<version>/lib/...`. That makes every installed keg an
immutable artifact: any number of versions coexist, and upgrading one package
can't break another. Per-project environments fall out of this almost for
free. A `dram.lock` pins exact versions plus the bottle sha256 for every
platform tag, and an environment is just a directory of symlinks into the
shared Cellar. It's maybe 10% of Nix's model for what felt like 90% of the
everyday value.

## Wave-scheduled installs in tokio

The install pipeline is the fun concurrency problem. Downloads can all run in
parallel, but a keg can only be extracted and relocated after its
dependencies are in place, and relocation is fork-heavy (one
`install_name_tool` plus one `codesign` per binary), so you want independent
DAG branches running concurrently too.

The scheduler ended up as one `tokio::select!` loop over two sources: a
`buffer_unordered(6)` stream of downloads, and a `JoinSet` of pour tasks
(each a `spawn_blocking`, gated by a `Semaphore` of 4). When a download
finishes, its keg pours immediately if its in-set dependency count is zero;
when a pour finishes, it decrements its dependents' counts and wakes any that
became ready. Pours overlap the remaining downloads, and a small `Mutex`
serializes only the writes to the shared `bin/` and `opt/` namespaces.
Everything keg-local runs parallel.

The result: node's full 20-formula tree, downloaded, extracted, relocated,
re-signed, and post-installed, in 4.2 seconds.

## post_install without Ruby

I assumed post-install logic would be locked up in formula Ruby code and
dram would have to skip it. It turns out Homebrew now publishes
`post_install_steps` as declarative JSON in the API: typed steps like
`mkdir_p`, `symlink`, `run` (with env and chdir), `install_gzipped_executable`,
guarded by conditions. So dram grew a small interpreter instead of a Ruby
dependency.

Real formulae then spent the afternoon teaching us what the spec actually
means:

- brew's `copy` and `symlink` steps have cp/ln semantics: an existing
  directory target means "put it inside," not "replace it." The literal
  reading dumped npm's guts directly into `lib/node_modules`.
- `set_permissions` isn't always octal. python@3.14 uses symbolic modes like
  `u+w`, so the interpreter needed a tiny chmod-spec parser.
- Steps carry platform guards (`{"condition": "on", "value": "linux"}`), and
  treating unknown guards as "pass" meant Linux-only steps were quietly
  running on macOS.
- An empty file in the openssl bottle crashed the placeholder scan, because
  `bytes.windows(0)` panics in Rust.
- Java `.class` files share the `0xcafebabe` magic with fat Mach-O binaries,
  so "is this a Mach-O" needs to tolerate `otool` rejecting the file.
- Piping `dram ls | head` panicked until SIGPIPE handling was reset to the
  Unix default, since Rust ignores SIGPIPE and turns it into a broken-pipe
  panic in println.

Every one of those came from installing real packages and watching what
broke, not from reading specs.

## What the human actually did

Since this is the disclosure post: the model wrote nearly all of the Rust.
What I did was direct, veto, and break things. I chose the goals and the
scope cuts, rejected UI designs until the output looked right (the sticky
status line went through three iterations), caught a regression where
dependency attribution silently vanished from the install output, asked the
"wait, would brew's default config change the benchmark numbers" question
that kept the benchmarks honest, and ran everything on my machine all day.
The bugs above were found because I kept installing things and pasting the
failures back.

I don't think either half works alone. The model knew things I didn't, like
Mach-O load command formats and that declarative post_install JSON existed at
all. I supplied the taste, the skepticism, and the willingness to say "that's
ugly, try again."

## Numbers

Same machine, same network, cold caches, brew given `HOMEBREW_NO_AUTO_UPDATE=1`
(its best case):

| Operation | brew | dram |
|---|---|---|
| Cold install: ffmpeg tree (13 formulae) | 6.4s | 3.3s |
| No-op install | 0.57s | 0.07s |
| Uninstall ffmpeg tree + autoremove | 1.5s | 0.3s |

Scope is deliberately narrow: bottles only, no casks, no source builds, no
services, Apple silicon only so far. Brew still does the rest, and dram
free-rides on infrastructure the Homebrew project builds and pays for, which
deserves saying plainly.

Code, benchmarks, and the honest limits list: [github.com/spoodyz/dram](https://github.com/spoodyz/dram)
