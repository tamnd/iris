# iris-guard

Structural validation of Arrow arrays crossing the sandbox boundary.

A sandboxed decoder returns offsets, lengths and validity bitmaps that the host is about to trust. This crate checks them first, and the cost of checking them is a published number.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
