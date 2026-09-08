"""Where the container the tests open comes from.

These tests run in two places and the difference is the whole point of them. On a developer machine
and in the workspace CI there is a Rust toolchain, so the fixture is written on the spot. In the
clean machine job there is not: the wheel is installed from a file, the toolchain has been deleted,
and the container arrives as an artefact whose path is in `IRIS_SAMPLE`. The same tests run either
way, which is what makes the second run mean something.
"""

import os
import pathlib
import shutil
import subprocess

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[3]

ROWS = 1000
COLUMNS = 4


@pytest.fixture(scope="session")
def sample(tmp_path_factory):
    """A container of a thousand rows and four columns, as a path."""
    named = os.environ.get("IRIS_SAMPLE")
    if named:
        return pathlib.Path(named)

    if shutil.which("cargo") is None:
        pytest.skip("no cargo to write a container with, and IRIS_SAMPLE names no other one")

    path = tmp_path_factory.mktemp("iris") / "sample.iris"
    subprocess.run(
        [
            "cargo",
            "run",
            "--release",
            "--locked",
            "-p",
            "iris-runtime",
            "--example",
            "write_container",
            "--",
            str(path),
            "--rows",
            str(ROWS),
            "--columns",
            str(COLUMNS),
        ],
        cwd=ROOT,
        check=True,
    )
    return path


@pytest.fixture(scope="session")
def sample_bytes(sample):
    """The same container, in memory."""
    return sample.read_bytes()
