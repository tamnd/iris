/* The C ABI for iris.
 *
 * Everything with structure in it comes back as an Arrow C structure, so this header is five
 * functions that hand out handles, five that use them, and no description of what a column is. A
 * consumer that already reads Arrow reads the output of this library with no glue at all.
 *
 * docs/C_ABI.md is the guide and examples/scan.c is a complete program.
 *
 * Errors. Every call that can fail returns an int32_t status and takes a char **error last. On
 * IRIS_ERROR the message is written there and belongs to the caller, who releases it with
 * iris_string_free. On IRIS_OK nothing is written. Passing NULL for error is allowed and the status
 * still comes back. There is no iris_last_error, on purpose: a last error slot is per thread state,
 * and this project does not have any.
 *
 * Ownership. A handle from this library is freed by this library. iris_open copies the bytes it is
 * given, so the caller may release its buffer as soon as the call returns. An ArrowSchema or an
 * ArrowArrayStream filled in here is released by calling its own release member, which is the Arrow
 * rule rather than one of ours. A dataset keeps its runtime alive, so freeing the runtime handle
 * while a dataset is open is allowed.
 *
 * Threads. A runtime and a dataset may both be used from several threads at once. Nothing here is
 * pinned to the thread it was made on.
 *
 * Copyright the iris contributors. Licensed under Apache-2.0.
 */

#ifndef IRIS_H
#define IRIS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The Arrow C data interface, reproduced under the guards the Arrow specification asks for so that
 * this header composes with any other one that also carries them. If you have already included
 * arrow's abi.h, or a header from another library that carries these, the definitions below are
 * skipped and the two agree, because there is only one definition of them to have.
 */
#ifndef ARROW_C_DATA_INTERFACE
#define ARROW_C_DATA_INTERFACE

#define ARROW_FLAG_DICTIONARY_ORDERED 1
#define ARROW_FLAG_NULLABLE 2
#define ARROW_FLAG_MAP_KEYS_SORTED 4

struct ArrowSchema {
  const char *format;
  const char *name;
  const char *metadata;
  int64_t flags;
  int64_t n_children;
  struct ArrowSchema **children;
  struct ArrowSchema *dictionary;

  void (*release)(struct ArrowSchema *);
  void *private_data;
};

struct ArrowArray {
  int64_t length;
  int64_t null_count;
  int64_t offset;
  int64_t n_buffers;
  int64_t n_children;
  const void **buffers;
  struct ArrowArray **children;
  struct ArrowArray *dictionary;

  void (*release)(struct ArrowArray *);
  void *private_data;
};

#endif /* ARROW_C_DATA_INTERFACE */

#ifndef ARROW_C_STREAM_INTERFACE
#define ARROW_C_STREAM_INTERFACE

struct ArrowArrayStream {
  int (*get_schema)(struct ArrowArrayStream *, struct ArrowSchema *out);
  int (*get_next)(struct ArrowArrayStream *, struct ArrowArray *out);
  const char *(*get_last_error)(struct ArrowArrayStream *);

  void (*release)(struct ArrowArrayStream *);
  void *private_data;
};

#endif /* ARROW_C_STREAM_INTERFACE */

/* The call succeeded. Nothing was written to the error argument. */
#define IRIS_OK 0

/* The call failed and the reason is in the error argument, unless that was NULL. */
#define IRIS_ERROR 1

/* An argument the call cannot do without was NULL, so nothing was attempted and there is no
 * message. This is a bug in the caller rather than a condition, which is why it is separate.
 */
#define IRIS_INVALID 2

/* A runtime, which is where compiled decoders are held. One is enough for a process. */
typedef struct IrisRuntime IrisRuntime;

/* An open container. */
typedef struct IrisDataset IrisDataset;

/* The version of iris this library was built from. Valid for the life of the process, not freed. */
const char *iris_version(void);

/* Makes a runtime. NULL if the engine could not be built. */
IrisRuntime *iris_runtime_new(void);

/* Releases a runtime handle. NULL is accepted and does nothing. */
void iris_runtime_free(IrisRuntime *runtime);

/* Names a directory to keep compiled decoders in, which is off until this is called.
 *
 * A warm open is a fraction of a millisecond against tens of milliseconds for one that compiles,
 * and docs/COLD_START.md has the numbers. The directory holds machine code, so one another user can
 * write into is one that can hand this process anything, and choosing it is an operator's decision.
 *
 * It applies to datasets opened afterwards. Call it before opening anything.
 */
int32_t iris_runtime_set_compilation_cache(IrisRuntime *runtime, const char *dir, char **error);

/* Opens a container held in memory. The bytes are copied. */
int32_t iris_open(const IrisRuntime *runtime, const uint8_t *bytes, size_t len, IrisDataset **out,
                  char **error);

/* Opens a container in a file, read whole. */
int32_t iris_open_path(const IrisRuntime *runtime, const char *path, IrisDataset **out,
                       char **error);

/* Releases a dataset handle. NULL is accepted and does nothing. */
void iris_dataset_free(IrisDataset *dataset);

/* The name the container carries. Points into the dataset and is valid until it is freed. */
const char *iris_dataset_name(const IrisDataset *dataset);

/* Fills in an ArrowSchema the caller owns and releases through its own release member. */
int32_t iris_dataset_schema(const IrisDataset *dataset, struct ArrowSchema *out, char **error);

/* Scans every row of every column.
 *
 * The scan happens inside this call, so what comes back is a stream over batches that are already
 * decoded and nothing done with the stream afterwards can fail for a reason to do with iris.
 */
int32_t iris_dataset_scan(const IrisDataset *dataset, struct ArrowArrayStream *out, char **error);

/* Scans the named columns, by position in the schema. A count of zero reads every column.
 *
 * A decoder that agreed to projection fetches the bytes of those columns and no others. One that
 * did not has every column read and the wanted ones taken out afterwards. Same answer, fewer bytes.
 */
int32_t iris_dataset_scan_columns(const IrisDataset *dataset, const uint32_t *columns, size_t count,
                                  struct ArrowArrayStream *out, char **error);

/* Releases a message this library wrote to an error argument. NULL is accepted and does nothing. */
void iris_string_free(char *message);

#ifdef __cplusplus
}
#endif

#endif /* IRIS_H */
