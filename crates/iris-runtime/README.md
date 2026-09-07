# iris-runtime

The iris runtime an engine embeds.

Ties the format, the virtual machine, the source, the guard and the native table into a scan an engine can pull batches from.

A decoder is hashed and compared to what the container says before it is compiled, and there is no way to reach the module that skips it. By default the module has to be inside the container: a dataset that names a decoder by URI is asking the host to fetch something and then run it, so that fails closed until an operator passes a `Policy` carrying a resolver they wrote. Whatever the resolver returns is hashed the same way.

Every scan records one `tracing` event at debug level on the target `iris::scan`, saying whether the sandbox or a registered native implementation read the rows, naming the decoder digest either way and the kernel digest and implementation when one stood in. Nothing is installed to receive it, because where a host's logs go is the host's to decide.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
