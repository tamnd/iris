# iris-abi

The guest and host ABI for iris self-decoding datasets.

The ABI is the only surface in iris that can ossify, so it is shaped like a wire protocol: length prefixed records, negotiated capabilities, and a defined way for either side to refuse politely.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
