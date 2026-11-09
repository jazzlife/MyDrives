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
