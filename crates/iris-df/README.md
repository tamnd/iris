# iris-df

A DataFusion table provider that reads iris datasets.

Registers a container as a table, splits a scan into tuple ranges DataFusion runs in parallel, and pushes a projection through to the decoder so a query that reads three of forty columns moves three fortieths of the bytes.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
