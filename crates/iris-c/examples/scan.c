/* Opens an iris container and prints what is in it.
 *
 * This is the whole of what a C program has to do, and it is also the program the clean machine gate
 * compiles and runs on each of the three desktop platforms with nothing installed but a compiler.
 *
 *   cc -I include scan.c -L . -liris -o scan
 *   ./scan sample.iris
 *
 * It prints the container name, the columns and their Arrow format strings, and the number of rows
 * the scan produced. It does not decode a single value, because reading values out of an Arrow array
 * is the job of whatever Arrow library the caller already has, and this program deliberately has
 * none.
 *
 * Copyright the iris contributors. Licensed under Apache-2.0.
 */

#include <stdio.h>
#include <stdlib.h>

#include "iris.h"

/* Prints a message this library handed back and releases it. */
static void complain(const char *what, char *error) {
  fprintf(stderr, "%s: %s\n", what, error ? error : "no message");
  iris_string_free(error);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <container>\n", argv[0]);
    return 2;
  }

  printf("iris %s\n", iris_version());

  IrisRuntime *runtime = iris_runtime_new();
  if (!runtime) {
    fprintf(stderr, "the runtime could not be built\n");
    return 1;
  }

  char *error = NULL;
  IrisDataset *dataset = NULL;
  if (iris_open_path(runtime, argv[1], &dataset, &error) != IRIS_OK) {
    complain(argv[1], error);
    iris_runtime_free(runtime);
    return 1;
  }

  printf("name: %s\n", iris_dataset_name(dataset));

  struct ArrowSchema schema;
  if (iris_dataset_schema(dataset, &schema, &error) != IRIS_OK) {
    complain("schema", error);
    iris_dataset_free(dataset);
    iris_runtime_free(runtime);
    return 1;
  }

  printf("columns: %lld\n", (long long)schema.n_children);
  for (int64_t i = 0; i < schema.n_children; i++) {
    struct ArrowSchema *child = schema.children[i];
    printf("  %s %s\n", child->name ? child->name : "?", child->format);
  }
  schema.release(&schema);

  struct ArrowArrayStream stream;
  if (iris_dataset_scan(dataset, &stream, &error) != IRIS_OK) {
    complain("scan", error);
    iris_dataset_free(dataset);
    iris_runtime_free(runtime);
    return 1;
  }

  long long rows = 0;
  long long batches = 0;
  for (;;) {
    struct ArrowArray array;
    if (stream.get_next(&stream, &array) != 0) {
      const char *last = stream.get_last_error(&stream);
      fprintf(stderr, "stream: %s\n", last ? last : "no message");
      stream.release(&stream);
      iris_dataset_free(dataset);
      iris_runtime_free(runtime);
      return 1;
    }
    /* A released array is how the stream says there are no more. */
    if (!array.release) {
      break;
    }
    rows += (long long)array.length;
    batches++;
    array.release(&array);
  }
  stream.release(&stream);

  printf("rows: %lld in %lld batches\n", rows, batches);

  iris_dataset_free(dataset);
  iris_runtime_free(runtime);
  return 0;
}
