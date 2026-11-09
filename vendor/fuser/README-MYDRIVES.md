# fuser, vendored

Upstream: [`fuser` 0.18.0](https://crates.io/crates/fuser) (MIT). Copied here,
not forked on GitHub, so the change is visible in the same review as the code
that depends on it.

## Why

On macOS `fuser` speaks **macFUSE's** dialect of the FUSE protocol: `fuse_attr`,
`fuse_setattr_in` and the xattr structs carry extra fields (`crtime`,
`bkuptime`, `chgtime`, `flags`, xattr `position`). **FUSE-T** - the
implementation that needs no kernel extension, and the only way a MyDrives
volume reaches Finder's sidebar - uses the **Linux** layout instead.

The difference is 12 bytes in `fuse_attr` (`crtime` + `crtimensec`), and it is
not a theory: sending sentinel values showed FUSE-T reading our `mode` as its
`gid`, our `nlink` as its `rdev`, our `uid` as its `blksize`. The mount then
hangs on the first lookup, because the root does not look like a directory.

## The change

One cargo feature, `apple-abi`, off by default:

* **off** - Linux layout, which is what FUSE-T expects (and what MyDrives
  builds with).
* **on** - the original macFUSE layout, for a macFUSE build.

It guards exactly the fields that differ, plus the code that fills and reads
them (`ll/fuse_abi.rs`, `ll/reply.rs`, `ll/request.rs`). Everything else is
upstream, unmodified.

## Updating

Copy a newer `fuser` over this directory and re-apply the `apple_abi` cfgs;
`git diff` against the release tarball shows the whole patch.

## Two more FUSE-T quirks, same fork

* **`macfuse-4-compat`** - upstream's `build.rs` turns this on for every macOS
  build, which widens `fuse_rename_in` to `newdir + flags + padding`. FUSE-T
  sends the plain 8-byte form, so reading 16 bytes ate the first eight bytes of
  the file name (`mv-src.txt` arrived as `xt`), the request failed to parse and
  every rename came back as `ENOSYS` - `mv`, `ditto` and `rsync` all reported
  "Permission denied". It now follows `apple-abi`.
* **Socket framing** (`channel.rs`) - `/dev/fuse` hands over exactly one
  request per `read`, but FUSE-T's channel is a stream socket and may deliver a
  32 KiB write in pieces. Upstream then logs "Short read of FUSE request
  (80 < 32848)" and the write hangs; `receive_rest` reads until the length in
  the header is satisfied.
