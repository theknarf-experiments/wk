# Patches

Genuine diffs against upstream. Everything else this port adds lives outside
this directory as ordinary files, because it belongs to an extension point
upstream already has:

* `../ace/config-wasi.h` and `../ace/platform_wasi.GNU` — ACE ships one
  `config-<platform>.h` / `platform_<platform>.GNU` pair per platform, so
  adding WASI is not a modification of ACE.
* `../shim/include/` — declarations wasi-libc withholds (`struct cmsghdr` and
  the `CMSG_*` family, `sendmsg`/`recvmsg`, `<net/if.h>`), supplied rather than
  carving the code that needs them out of ACE.
* `../shim/*.c` — the threading policy and the registry that replaces a thread,
  on the link line rather than as `#ifdef`s across two upstream trees.

## Naming

`<tree>-NNNN-<what>.patch`, applied in order by `../build-target.sh`:
`opendds-*` first (OpenDDS is present from `../fetch.sh`), then `ace-*` after
configure, because `ACE_wrappers/` does not exist until configure downloads it.

Application is idempotent — an already-applied patch is skipped, not re-applied
and not an error — so `build-target.sh` can be re-run freely.

## Regenerating one

The patches are plain `diff -u` against a pristine copy of each file, with the
headers rewritten to paths relative to `src/OpenDDS`. To change one, edit the
file in `../src/OpenDDS/`, then re-diff it against a pristine copy of the same
file from the pinned tag. **Re-diff before building**: `build-target.sh` proves
each patch is applied by `git apply --reverse --check`, so a tree edited beyond
what the patch file says fails that check and the build stops — which is the
intent (the patch file, not the working tree, is the artifact).

## Every patch is guarded

Every hunk that changes behaviour is inside `#ifdef ACE_WASI` (defined by
`../ace/config-wasi.h`, following ACE's own convention of a platform config
announcing its platform), with **one deliberate exception**:

* `ace-0002-max-handles-indeterminate.patch` is unguarded, because it is not a
  WASI adaptation — it is a plain bug. POSIX says a `sysconf()` that returns -1
  without setting `errno` means "indeterminate", and ACE was handing that -1
  back to callers as a count. The fix is correct on every platform.

`ace-0001-wasi-fd-set.patch` is a middle case worth noting: it is unguarded
where it generalises (the hard-coded `fd_count`/`fd_array` member names become
`ACE_FD_SET_COUNT`/`ACE_FD_SET_ARRAY`, defaulting to exactly what they were,
and the array iterator walks downward so that removing an element mid-walk is
safe), and guarded where it is a claim about WASI specifically.
