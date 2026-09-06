# The remote scan comparison

The M5 question, from the roadmap decision points: does declaring ranges actually beat a well configured Parquet reader on bytes transferred and on latency, against real object storage?

The short answer is no, and this document is the long one. Declaring ranges moves the same bytes as a page indexed Parquet reader over an uncompressed file, to within one percent. Against a compressed one it moves nearly three times as many. On requests a bare range source is far behind and host side coalescing brings it slightly ahead. On wall clock it is behind on every shape measured. The design notes that treated transfer volume as the differentiator were wrong, and the differentiator that is left is that the decoder travels with the data.

The probe that produced this is `crates/iris-runtime/examples/remote_scan.rs`, behind the off by default `probe` feature. Everything below can be reproduced from it.

## What was compared

One table, written two ways, read over the same S3 endpoint through the same counting instrument.

The table is 250,000 rows of 40 non-nullable `int64` columns, where the value at column `c` and row `r` is `c * 1_000_000_000 + r`. Two shapes are read from it: every column, and columns 7, 19 and 31, which are spread across the table rather than adjacent so that a reader which quietly rounds a run of ranges up to something convenient cannot pass by fetching one contiguous stretch.

The iris side is an `iris` container holding the `fixedwidth` decoder, read through `ObjectSource`, which asks the endpoint for exactly the byte ranges the decoder declares. It appears twice. `iris` is that source on its own. `iris-ahead` is the same source under `Readahead` with a one mebibyte block and one held block per column, which is the host side coalescer M4 built for exactly this access pattern. Both are reported because a bare source is not what a host would deploy, and comparing a tuned Parquet reader against an untuned iris would be a strawman in the other direction.

The Parquet side is the `parquet` crate's asynchronous reader with the page index required, so it fetches page ranges rather than whole column chunks. It also appears twice. `parquet-plain` is written with no compression and no dictionary, so its column chunks are the same size as the container's columns and what is left between the two sides is the mechanism. `parquet-tuned` is written with zstd level 3 and dictionaries on, which is what a deployment actually writes. Both are needed: a byte count against a compressed file measures the codec, and a byte count against an uncompressed one is not what anybody runs.

Both sides write 65,536 row row groups and read 8,192 row batches, so neither is being compared against a different chunking decision.

Both sides fetch through one `ObjectStore` wrapper that counts requests when they are made and bytes when they arrive. That is why the byte counts are comparable: they were counted in one place, by one piece of code, rather than by each library about itself. The probe deliberately does not delegate `get_ranges`, so a batched fetch is counted as the individual requests it becomes on the wire. It also does not count the body of a `HEAD`, which has none, even though the store fills in the range it would have served.

Each side is also measured opening and stopping there, reported as the `open` shape. iris pays for the trailer, the header, the footer, the decoder section, the hash of that section and compiling it. Parquet pays for the footer and the page index. Neither cost is visible in a scan number that has it folded in, and it turned out to be a large part of the difference between the two sides.

Every repeat opens its reader cold, so a repeat pays what a first query pays rather than what a second one does.

## Where the numbers come from

Apple M4, ten cores, 24 GiB, macOS 15.8, aarch64. Release profile, which in this workspace is fat LTO and one codegen unit.

The endpoint is MinIO `RELEASE.2025-09-07T16-13-09Z` running under Docker Desktop, reached over loopback at `127.0.0.1`. The median round trip to it, taken as forty one byte fetches, is 619 microseconds. That is what one saved request is worth here.

Loopback understates that badly, and the arithmetic below says by how much. A bucket in the same region as the reader is more like one to twenty milliseconds away, so the request counts matter between one and thirty times more in a real deployment than the wall clock column here makes them look. The byte counts and the request counts do not depend on the endpoint being close, which is why they are the part of this result worth quoting.

Five repeats, median reported, with the fastest and slowest of the five next to it.

The same probe runs on `epyc-8c-24gb` in the fleet workflow and uploads its JSON. That run exists to check that the counters are a property of the mechanism rather than of one operating system and one architecture. Its durations are not published, because that machine is a shared tenancy virtual machine and a duration measured next to an unknown neighbour is not evidence.

## The numbers

Object sizes, for the same 250,000 by 40 table.

| Object | Bytes |
|---|---|
| iris container | 80,075,656 |
| parquet-plain | 80,049,514 |
| parquet-tuned | 29,146,137 |

Requests and bytes over the socket, and wall clock, per read.

| Side | Shape | Requests | Bytes | Median ms | Spread ms |
|---|---|---|---|---|---|
| iris | open | 5 | 75,639 | 34.2 | 33.6 to 40.2 |
| iris | all 40 | 1,246 | 80,075,655 | 1313.0 | 1148.9 to 1384.8 |
| iris | 3 of 40 | 99 | 6,075,655 | 163.8 | 156.0 to 177.4 |
| iris-ahead | all 40 | 84 | 83,942,592 | 392.6 | 328.5 to 575.7 |
| iris-ahead | 3 of 40 | 11 | 7,418,072 | 125.4 | 92.0 to 134.0 |
| parquet-plain | open | 3 | 38,510 | 2.2 | 2.1 to 2.5 |
| parquet-plain | all 40 | 163 | 80,049,510 | 270.4 | 263.4 to 292.2 |
| parquet-plain | 3 of 40 | 15 | 6,039,335 | 28.2 | 27.3 to 38.5 |
| parquet-tuned | open | 3 | 40,534 | 2.1 | 2.1 to 2.7 |
| parquet-tuned | all 40 | 163 | 29,146,133 | 274.4 | 264.3 to 299.1 |
| parquet-tuned | 3 of 40 | 15 | 2,220,863 | 25.9 | 24.6 to 32.5 |

Every scan returned 250,000 rows, and the three projected columns were compared value by value against the container's own, 1,500,000 values in total, so all four sides are reading the same table.

## What it says

**The projection pushdown is exact.** A bare iris projected scan moves 6,075,655 bytes out of an 80,075,656 byte object, which is 7.59 percent for three columns out of forty. Three fortieths is 7.5 percent. The remaining tenth of a percent is the fixed part of the container that every scan reads. This is the M5 gate item that issue #30 closed, restated here against a different fixture.

**On bytes it is a tie with an uncompressed Parquet reader, not a win.** 6,075,655 against parquet-plain's 6,039,335 is a difference of 0.6 percent, in Parquet's favour. There is no mechanism advantage here to find. Parquet's row groups and page index already declare ranges as precisely as a declared range source can, and have done for years. The thing the design notes assumed was distinctive is not.

**On bytes against a compressed Parquet reader it is a clear loss.** 6,075,655 against parquet-tuned's 2,220,863 is 2.74 times as many bytes. That gap is entirely the codec. The container in this fixture holds the `fixedwidth` decoder, which stores little endian `int64` values and compresses nothing, so what the comparison is showing is zstd and a dictionary against nothing at all. It is still the honest number to publish, because what a deployment compares against is a compressed Parquet file and not a hypothetical one. What it is not is evidence about ranges.

**On requests a bare range source loses badly and coalescing fixes it.** 99 requests against 15 for the projected shape, and 1,246 against 163 for the full one. The decoder asks for a batch of one column at a time and each of those is a round trip. With `Readahead` on, the projected shape drops to 11 requests, which is fewer than Parquet's 15, at the cost of 22 percent more bytes. That trade is the whole of what host side coalescing does, stated in two numbers.

At 619 microseconds a round trip those request counts barely register. At a regional twenty milliseconds they dominate: the bare source would spend about two seconds in round trips for the projected scan, Parquet about 0.3 seconds and coalesced iris about 0.22 seconds. So over a distant endpoint the ranking on latency flips in coalesced iris's favour while the ranking on bytes stays against it, and which one wins is a bandwidth delay product question rather than a design question. This report cannot answer that from loopback and does not try.

**On wall clock it is behind on every shape.** The projected scan is 125.4 ms coalesced against 25.9 ms for parquet-tuned. About 34 of those milliseconds are the open, and only about three of those 34 are the network, by the round trip figure and the five requests in the table. The other 31 are local work: parsing the footer, hashing the decoder section and compiling it to machine code. There is no compiled module cache in `iris-runtime` today, so every `open_windowed` pays that again, and the hash that would key such a cache is already being computed for verification. That is a fixed cost per open and it is the single largest addressable item in this table.

Taking the open out of both sides leaves roughly 91 ms against 24 ms for the projected shape, so the scan itself is about four times slower rather than five, and the gap there is decoding through a sandbox against decoding in process. The guard cost measurement in M2 already put a number on part of that.

## The answer to the decision point

The roadmap asked what to do if declared ranges bought nothing: "the main differentiator is gone and the design notes are wrong". That is the outcome. Declared ranges are not a transfer volume advantage over Parquet, they are a tie at best, and the notes that said otherwise were comparing against a Parquet reader that did not use its page index.

What survives is the thing the range mechanism was built to enable rather than the mechanism itself. An `iris` container carries its own decoder, so a reader that has never seen the encoding can still read the file, and it can read it through a window smaller than the file without the decoder ever addressing the whole thing. Neither of those is a byte count claim. The project's case rests on them and not on transfer volume, and the README and the design notes should say so.

## What this does not settle

One fixture shape. Forty `int64` columns of a highly regular arithmetic sequence, which is close to the best case for a dictionary and for zstd, so the codec gap here is probably wider than it would be on messier data.

One decoder. The container holds `fixedwidth`, which does no encoding at all. The BtrBlocks decoder that M5 item #29 landed is the one a real container would carry, and re-running this against it is the direct way to find out how much of the 2.74 times byte gap is the codec and how much is the format. That comparison is worth doing before anyone quotes the number above as a format result.

One readahead depth, one mebibyte, with one held block per column. The over-fetch a coalescer costs is its block size divided by the run length it is coalescing, so that ratio is a tuning question this probe fixes rather than explores.

One endpoint, on loopback, in the same process tree as the reader. Latency, bandwidth and tail behaviour against a real bucket over a real network are all unmeasured here, and the paragraph above about twenty milliseconds is arithmetic rather than a measurement.

## Reproducing it

An S3 compatible endpoint and five environment variables, the same five the portability gate reads.

```
docker run -d --name iris-probe-minio -p 9010:9000 \
  -e MINIO_ROOT_USER=irisgate -e MINIO_ROOT_PASSWORD=irisgatesecret \
  quay.io/minio/minio server /data
docker exec iris-probe-minio mc alias set gate http://127.0.0.1:9000 irisgate irisgatesecret
docker exec iris-probe-minio mc mb --ignore-existing gate/iris-gate
```

```
AWS_ENDPOINT_URL=http://127.0.0.1:9010 \
AWS_ACCESS_KEY_ID=irisgate \
AWS_SECRET_ACCESS_KEY=irisgatesecret \
AWS_REGION=us-east-1 \
IRIS_TEST_BUCKET=iris-gate \
cargo run --release -p iris-runtime --features probe --example remote_scan -- \
  --rows 250000 --repeats 5
```

`--json` emits the same run as one object, which is what the fleet workflow uploads. The probe builds its own fixtures, uploads them, and deletes them on the way out.
