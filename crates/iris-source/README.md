# iris-source

Range oriented data sources for iris.

A decoder declares the byte ranges it needs and the host serves them. That inversion is what lets the same decoder run against a local file, a page cache, and an object store.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
