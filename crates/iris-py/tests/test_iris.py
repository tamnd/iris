"""What the bindings promise, checked the way a user would find out.

Every test here goes through the installed wheel and through pyarrow, because the claim is not that
the Rust underneath works, which the workspace tests cover, but that a Python program gets Arrow data
out of it without writing any glue.
"""

import pathlib

import pyarrow as pa
import pytest

import iris

from conftest import COLUMNS, ROWS


def cell(column, row):
    """The value the fixture's decoder writes at a position, which the Rust support module sets."""
    return column * 1_000_000_000 + row


@pytest.fixture
def runtime():
    return iris.Runtime()


@pytest.fixture
def dataset(runtime, sample):
    return runtime.open_path(sample)


def test_the_version_is_the_one_this_was_built_from():
    assert iris.__version__.count(".") == 2


def test_a_container_in_a_file_opens_and_reads(dataset):
    assert dataset.name == "readings"
    assert dataset.num_columns == COLUMNS
    assert dataset.column_names == [f"c{c}" for c in range(COLUMNS)]

    table = pa.table(dataset)
    assert table.num_rows == ROWS
    assert table.column_names == [f"c{c}" for c in range(COLUMNS)]
    assert table.column("c0").to_pylist() == [cell(0, row) for row in range(ROWS)]
    assert table.column("c3").to_pylist() == [cell(3, row) for row in range(ROWS)]


def test_a_container_in_memory_opens_the_same_way(runtime, sample_bytes):
    table = pa.table(runtime.open(sample_bytes))
    assert table.num_rows == ROWS
    assert table.num_columns == COLUMNS


def test_the_schema_comes_back_without_reading_anything(dataset):
    schema = pa.schema(dataset)
    assert schema.names == [f"c{c}" for c in range(COLUMNS)]
    assert all(field.type == pa.int64() for field in schema)


def test_a_projection_reads_the_columns_it_named(dataset):
    table = pa.table(dataset.scan([0, 2]))
    assert table.column_names == ["c0", "c2"]
    assert table.num_rows == ROWS
    assert table.column("c2").to_pylist() == [cell(2, row) for row in range(ROWS)]


def test_no_projection_is_every_column(dataset):
    assert pa.table(dataset.scan()).num_columns == COLUMNS
    assert pa.table(dataset.scan(None)).num_columns == COLUMNS


def test_a_scan_can_be_taken_twice(dataset):
    first = pa.table(dataset.scan([1]))
    second = pa.table(dataset.scan([1]))
    assert first.equals(second)


def test_a_scan_outlives_the_dataset_it_came_from(dataset):
    scan = dataset.scan([0])
    del dataset
    assert pa.table(scan).num_rows == ROWS


def test_the_buffers_are_not_copied_into_pyarrow(dataset):
    """The claim the issue asks for, checked against pyarrow's own allocator.

    An Arrow array that pyarrow imported through the C data interface points at memory somebody else
    allocated, so pyarrow's pool does not grow. An array pyarrow built by copying comes out of that
    pool and it does. Eight thousand values of eight bytes is thirty two kilobytes a copy would have
    to find somewhere, which is far outside any rounding this number does.
    """
    scan = dataset.scan()
    before = pa.total_allocated_bytes()
    table = pa.table(scan)
    after = pa.total_allocated_bytes()

    assert table.num_rows == ROWS
    assert after == before


def test_a_copy_would_have_shown_up_on_that_meter(dataset):
    """The other half of the test above, so that it is a measurement and not a constant.

    If importing and copying looked the same on this meter, the assertion above would pass whatever
    the bindings did. Casting a column to a narrower integer is a copy by definition, because the
    values end up in a buffer of a different width, and pyarrow has to find that buffer somewhere.
    """
    table = pa.table(dataset)
    before = pa.total_allocated_bytes()
    copied = table.column("c0").cast(pa.int32())
    assert pa.total_allocated_bytes() > before
    assert len(copied) == ROWS


def test_bytes_that_are_not_a_container_say_so(runtime):
    with pytest.raises(RuntimeError):
        runtime.open(b"not a container")


def test_a_file_that_is_not_there_names_itself(runtime, tmp_path):
    missing = tmp_path / "absent.iris"
    with pytest.raises(RuntimeError) as raised:
        runtime.open_path(missing)
    assert "absent.iris" in str(raised.value)


def test_a_requested_schema_is_refused_rather_than_ignored(dataset):
    with pytest.raises(NotImplementedError):
        dataset.__arrow_c_stream__(pa.schema([pa.field("c0", pa.int32())]))


def test_a_compilation_cache_is_named_through_the_bindings(sample, tmp_path):
    directory = tmp_path / "decoders"
    directory.mkdir()

    runtime = iris.Runtime()
    runtime.set_compilation_cache(str(directory))
    assert pa.table(runtime.open_path(sample)).num_rows == ROWS

    written = list(directory.rglob("*"))
    assert [entry for entry in written if entry.is_file()]


def test_a_path_can_be_a_string_or_a_path(runtime, sample):
    assert runtime.open_path(str(sample)).name == "readings"
    assert runtime.open_path(pathlib.Path(sample)).name == "readings"


def test_the_objects_say_what_they_are(runtime, dataset):
    assert repr(runtime) == "<iris.Runtime>"
    assert "readings" in repr(dataset)
    assert "every column" in repr(dataset.scan())
    assert "[0, 2]" in repr(dataset.scan([0, 2]))
