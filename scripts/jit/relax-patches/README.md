# Relax fixes Octos needs

These patches apply, in order, to [makepad/relax](https://github.com/makepad/relax)
at commit `442f13bc333ed5cb4b3d75de62693251acd39dd7`. Relax is MIT licensed
(Copyright (c) 2026 makepad); the patches modify its sources and carry the same
license. `scripts/jit/setup-relax.sh` fetches that commit and applies them.

They are here because Octos and octoscode do not run on that commit without
them, and they have not been sent upstream. Upstream moves quickly, so a newer
commit may already fix some of them. Moving the pin means checking each patch
against the new commit and repeating the checks in
[docs/JIT_DEV_MODE.md](../../../docs/JIT_DEV_MODE.md).

| Patch | What failed without it |
| --- | --- |
| 0001 | A program whose own sources belong to a built-in makepad macro package ended the run. Octos pins a makepad fork for `makepad-script`. |
| 0002 | A fn body that defines a `macro_rules!` it never calls was not parsed at load (`combine`). |
| 0003 | Two tokens of one macro expansion lost their order for hygiene: 2660 `E0425` errors in `combine` tuple parsers. |
| 0004 | An arm64 `asm!` block with more than two outputs was refused (`lzma-rust2`). |
| 0005 | Missing std methods: `CommandExt::uid`/`gid`, `PathBuf` and `OsString` capacity methods, `leak`, `add_extension`, `Path::file_prefix`, `Rc::pin`. |
| 0006 | A by-value method call or field move through a temporary `Box` dropped the contents twice or leaked them. `futures-util`'s `BiLock::unlock` does this with a `Waker`, which took one reference too many off a tokio task: the server panicked with `current: 1, sub: 2` in about 3 of 4 starts when a client connected at once. |
| 0007 | `relax inspect <pid> pin on` let the first edit after it through. |

Patches 0001 to 0006 each carry a test that fails on the unpatched commit.
Patch 0007 was checked by hand on a running program.
