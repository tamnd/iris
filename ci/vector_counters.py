#!/usr/bin/env python3
"""Instructions retired and instructions per cycle for the M5 vectorisation probe.

The probe in crates/iris-vm/examples/m5_vector.rs measures durations, which
answers how big the sandbox gap is and not what it is made of. The M5 gate asks
for the mechanism, and the mechanism needs an instruction count: a guest that
runs more instructions than the host is doing more work, and a guest that runs
the same instructions at a lower instructions per cycle is doing the same work
worse. Those are different findings with different fixes and a duration cannot
tell them apart.

How it gets a per iteration number without any counter API in the process:

Run the probe twice at two different repeat counts and subtract. Everything
that is not the timed loop appears identically in both runs. That includes the
nested cargo invocation that builds the guest modules, the Wasmtime compile,
reading the corpus off disk, and process startup and teardown. What is left in
the difference is exactly the extra iterations, so dividing by the extra
iterations and by the number of cases gives one decode of one part. No
instrumentation, nothing to get wrong in the probe, and it works the same on
any machine that can count anything at all.

Why the repeat counts are computed rather than fixed:

The subtraction only cancels the fixed part if the fixed part is the same in
both runs. Instructions retired very nearly are, because the same code runs.
Cycles are not: the fixed part is a nested cargo and two Wasmtime compiles,
which is a second or more of work whose cycle count moves with whatever else
the machine is doing. Subtract two of those and the noise that survives is
absolute, so it only becomes small next to a difference that is large. A fixed
repeat count cannot be large enough for a case that decodes in sixty
microseconds and small enough for one that takes twelve hundred, so the counts
are derived from a sizing run instead, aiming the difference at a target amount
of work. The first version of this used a fixed pair and returned a negative
cycle count on a shared machine, which is the noise being larger than the
signal, stated as plainly as a measurement can state it.

Each measurement is also taken several times and the smallest kept. Nothing
another tenant does makes a run faster, so the minimum is the run that was
interfered with least. The spread across those rounds is reported, and when the
cycle spread is wide enough that instructions per cycle would be a number about
the neighbours rather than about the code, this says so and does not print it.

Two tools, because one of them is not available where this most needs to run:

perf reads the hardware counters, which gives instructions and cycles and
therefore instructions per cycle. It needs a performance monitoring unit the
kernel is willing to expose, and a shared tenancy virtual machine often has
neither. callgrind counts instructions by simulating, so it needs no hardware
support at all and gives the same answer on every machine, but it cannot say
anything about cycles because it is not running on the real pipeline. Where
both work, perf is the one to quote and callgrind is a check on it. Where only
callgrind works, the instruction half of the answer is still available and the
instructions per cycle half is not, and this says so rather than inventing it.

Usage:

    ci/vector_counters.py [--cases NAME] [--tool auto|perf|callgrind]
                          [--seconds S] [--rounds N] [--json]

Everything it runs is release mode, and RUSTFLAGS is passed through untouched,
because which vector width the native side was allowed to use is the variable
the whole experiment turns on.

Exit codes: 0 when it measured something, 3 when no tool on this machine can
count. The second is a fact about the machine rather than a failure, and a
workflow that treats it as one is asking a virtual machine for a PMU.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# The built probe. Run directly rather than through `cargo run`, so that cargo's
# own work is not inside the thing being counted. It cancels in the subtraction
# either way, but only after it has already made the run slower and noisier.
#
# CARGO_TARGET_DIR is honoured because the two halves of this experiment differ
# by RUSTFLAGS, which invalidates a target directory, so running them out of one
# directory means rebuilding the world twice. A caller that gives each half its
# own directory keeps both.
TARGET = pathlib.Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
PROBE = TARGET / "release" / "examples" / "m5_vector"

# The three sides the probe knows, in the order they are reported.
SIDES = ("guest-simd128", "guest-scalar", "native")

# The hardware events. Two is enough for the question: how many instructions,
# and how well the machine issued them.
EVENTS = ("instructions", "cycles")

# callgrind prints its total as `I   refs:      12,345,678` at the end.
IREFS = re.compile(r"I\s+refs:\s+([\d,]+)")

# How much decoding the difference between the two runs should contain, in
# seconds of uninstrumented work, per tool.
#
# perf is measuring a real pipeline next to whatever else the machine is doing,
# so its difference has to be large enough to bury the run to run movement in
# the fixed part. callgrind simulates, so it returns the same instruction count
# every time and needs no margin at all, and since it costs upwards of fifty
# times the run time it gets a target that keeps a full sweep inside a CI job.
SECONDS = {"perf": 8.0, "callgrind": 0.2}

# How many times each measurement is taken before the smallest is kept.
# callgrind is deterministic, so a second round of it would return the first
# round's answer and cost another few minutes.
ROUNDS = {"perf": 3, "callgrind": 1}

# The smaller of the two repeat counts. Large enough that the caches and the
# branch predictors are in the state a steady loop leaves them in, small enough
# to be a small part of the run.
LOW = 50

# The floor on the difference, for a case so slow that the target amount of work
# would otherwise be reached in a handful of iterations. A per iteration number
# divided by a small integer inherits the quantisation of that integer.
MIN_DELTA = 50

# The relative spread across rounds beyond which cycles are reported as measured
# but not turned into instructions per cycle. Instruction counts hold to a
# fraction of a percent on any machine, so a wide spread is always the cycle
# side, and a ratio built on it would be a fact about the other tenants.
CYCLE_SPREAD_LIMIT = 0.10


def parse_perf(stderr: str) -> dict[str, int | None]:
    """Pulls the counts out of perf's comma separated output.

    The fields are value, unit, event, and then some run time bookkeeping. A
    value that is not a number is perf saying it could not count, and that stays
    None rather than becoming a zero.
    """
    counts: dict[str, int | None] = {event: None for event in EVENTS}
    for line in stderr.splitlines():
        fields = line.split(",")
        if len(fields) < 3:
            continue
        name = fields[2].split(":")[0]
        if name not in counts:
            continue
        try:
            counts[name] = int(fields[0])
        except ValueError:
            counts[name] = None
    return counts


def parse_callgrind(stderr: str) -> dict[str, int | None]:
    """What callgrind can report, in the same shape perf reports it."""
    match = IREFS.search(stderr)
    return {
        "instructions": int(match.group(1).replace(",", "")) if match else None,
        "cycles": None,
    }


def perf_works() -> tuple[bool, str]:
    """Whether perf is here and whether it can actually count.

    Being installed is not the same as working. Inside a virtual machine the
    kernel may expose no PMU at all, and perf_event_paranoid may refuse to let
    an unprivileged process read one that exists. Both show up as a successful
    run with a placeholder where the number should be, which is why this runs a
    real measurement against `true` rather than checking a version.
    """
    if shutil.which("perf") is None:
        return False, "perf is not installed"
    out = subprocess.run(
        ["perf", "stat", "-e", ",".join(EVENTS), "-x,", "true"],
        capture_output=True,
        text=True,
        check=False,
    )
    if any(value is None for value in parse_perf(out.stderr).values()):
        return False, (
            "perf ran but returned no counts, so this kernel is not exposing a "
            "performance monitoring unit to an unprivileged process. "
            "/proc/sys/kernel/perf_event_paranoid controls that, and on a virtual "
            "machine the counters may not be there to expose in the first place"
        )
    return True, "perf is present and counting"


def callgrind_works() -> tuple[bool, str]:
    """Whether valgrind is here. It needs no hardware support, so being installed is enough."""
    if shutil.which("valgrind") is None:
        return False, "valgrind is not installed"
    return True, "valgrind is present"


def measure(tool: str, side: str, repeats: int, cases: str | None) -> dict[str, int | None]:
    """One run of the probe under one tool, at one repeat count."""
    probe = [str(PROBE), "--only", side, "--repeats", str(repeats)]
    if cases:
        probe += ["--cases", cases]
    if tool == "perf":
        command = ["perf", "stat", "-e", ",".join(EVENTS), "-x,", *probe]
    else:
        # The output file goes nowhere because the per function breakdown is not
        # what this wants. The summary line on stderr is.
        command = [
            "valgrind",
            "--tool=callgrind",
            "--callgrind-out-file=/dev/null",
            *probe,
        ]
    out = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        # Valgrind decodes the instruction stream itself, and its decoder is not
        # the processor's. A build tuned for the exact machine can emit something
        # it has never been taught, and then it stops on that one instruction with
        # a page of register state that reads like a crash in the probe and is not
        # one. Naming it here is the difference between a five minute diagnosis
        # and an afternoon of looking at the wrong code.
        if "Unrecognised instruction" in out.stderr:
            sys.exit(
                "callgrind cannot decode this build. Something in it uses an instruction Valgrind "
                "does not implement, which is what a target-cpu tuned for the exact machine tends "
                "to produce. Build for a named target instead, so that what was measured is "
                "written down and Valgrind has heard of it."
            )
        sys.exit(f"the probe failed at {side} and {repeats} repeats:\n{out.stderr}")
    return parse_perf(out.stderr) if tool == "perf" else parse_callgrind(out.stderr)


def prime(cases: str | None) -> tuple[int, dict[str, float]]:
    """Builds everything, counts the cases, and sizes one pass over them.

    The first run of the probe compiles the guest modules with a nested cargo,
    and cargo itself will have the probe to build. Neither is part of what is
    being measured, and although the subtraction below cancels anything that
    happens identically in both runs, they are only identical once they have
    already happened. So they are made to happen here.

    The case count comes back because the difference between two runs is spread
    over every case the probe ran, and a per iteration number has to be divided
    by it. The seconds come back so that the repeat counts can be chosen to make
    that difference a useful size, which is different for a case that decodes in
    sixty microseconds and one that takes twelve hundred.
    """
    build = subprocess.run(
        ["cargo", "build", "--release", "--locked", "-q", "-p", "iris-vm", "--example", "m5_vector"],
        cwd=ROOT,
        check=False,
    )
    if build.returncode != 0:
        sys.exit("the probe did not build")
    command = [str(PROBE), "--repeats", "1", "--json"]
    if cases:
        command += ["--cases", cases]
    out = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        sys.exit(f"the probe failed while warming up:\n{out.stderr}")
    report = json.loads(out.stdout)
    seconds = {side: 0.0 for side in SIDES}
    for case in report["cases"]:
        for side in SIDES:
            seconds[side] += case["sides"][side]["median_us"] / 1e6
    return len(report["cases"]), seconds


def differences(
    tool: str, side: str, low: int, high: int, cases: str | None, rounds: int, count: int
) -> list[dict[str, float]]:
    """One pair of runs per round, differenced down to one decode of one part.

    The pair is what a round is, rather than all the low runs and then all the
    high ones, so that the two halves of a subtraction ran as close together in
    time as they can and saw as nearly as possible the same machine.
    """
    divisor = (high - low) * count
    taken = []
    for _ in range(rounds):
        small = measure(tool, side, low, cases)
        large = measure(tool, side, high, cases)
        if small["instructions"] is None or large["instructions"] is None:
            sys.exit(f"{tool} stopped counting partway through, at {side}")
        entry = {"instructions": (large["instructions"] - small["instructions"]) / divisor}
        if small["cycles"] is not None and large["cycles"] is not None:
            entry["cycles"] = (large["cycles"] - small["cycles"]) / divisor
        taken.append(entry)
    return taken


def spread(values: list[float]) -> float:
    """How far apart the rounds landed, as a fraction of the smallest."""
    low = min(values)
    return (max(values) - low) / low if low > 0 else float("inf")


def choose(requested: str) -> tuple[str | None, str]:
    """Picks the tool to count with, and says why the others were not it."""
    perf_ok, perf_why = perf_works()
    callgrind_ok, callgrind_why = callgrind_works()
    if requested == "perf":
        return ("perf", perf_why) if perf_ok else (None, perf_why)
    if requested == "callgrind":
        return ("callgrind", callgrind_why) if callgrind_ok else (None, callgrind_why)
    if perf_ok:
        return "perf", perf_why
    if callgrind_ok:
        return "callgrind", f"{perf_why}, so falling back: {callgrind_why}"
    return None, f"{perf_why}, and {callgrind_why}"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cases", help="only fixtures whose name contains this, or exactly matches it"
    )
    parser.add_argument("--tool", choices=("auto", "perf", "callgrind"), default="auto")
    parser.add_argument(
        "--seconds",
        type=float,
        help="how much decoding to put in the difference, in seconds of uninstrumented work",
    )
    parser.add_argument("--rounds", type=int, help="how many times to take each measurement")
    parser.add_argument("--json", action="store_true", help="emit one object instead of a table")
    parser.add_argument(
        "--json-out",
        type=pathlib.Path,
        help="also write that object here, so one run can feed both a reader and an artifact",
    )
    args = parser.parse_args()

    tool, why = choose(args.tool)
    if tool is None:
        # Not an error. A machine that cannot count is a machine this half of the
        # measurement does not happen on, and saying so is the honest outcome.
        print(f"Nothing here can count: {why}.")
        print("The duration half of the measurement still runs. This half does not.")
        sys.exit(3)

    seconds = SECONDS[tool] if args.seconds is None else args.seconds
    rounds = ROUNDS[tool] if args.rounds is None else args.rounds
    count, per_pass = prime(args.cases)

    rows = []
    for side in SIDES:
        delta = max(MIN_DELTA, math.ceil(seconds / per_pass[side])) if per_pass[side] > 0 else MIN_DELTA
        high = LOW + delta
        taken = differences(tool, side, LOW, high, args.cases, rounds, count)

        counted = [entry["instructions"] for entry in taken]
        row = {
            "side": side,
            "repeats": [LOW, high],
            "rounds": rounds,
            "instructions_per_iteration": min(counted),
            "instructions_spread": spread(counted),
        }
        cycled = [entry["cycles"] for entry in taken if "cycles" in entry]
        if len(cycled) == len(taken) and all(value > 0 for value in cycled):
            row["cycles_per_iteration"] = min(cycled)
            row["cycles_spread"] = spread(cycled)
        rows.append(row)

    # Instructions per cycle is only formed where every side's cycle count held
    # still across its rounds. One unstable side is enough to make the column a
    # comparison between a measurement and an accident.
    steady = all(
        row.get("cycles_spread") is not None and row["cycles_spread"] <= CYCLE_SPREAD_LIMIT
        for row in rows
    )
    for row in rows:
        if steady:
            row["instructions_per_cycle"] = (
                row["instructions_per_iteration"] / row["cycles_per_iteration"]
            )

    report = json.dumps(
        {
            "probe": "m5_vector_counters",
            "tool": tool,
            "cases": count,
            "case_filter": args.cases,
            "target_seconds": seconds,
            "cycles_steady": steady,
            "sides": rows,
        }
    )
    if args.json_out:
        args.json_out.write_text(report + "\n", encoding="utf-8")
    if args.json:
        print(report)
        return

    print("Instructions per decode of one part, from the difference between two runs")
    print()
    print(f"Tool: {tool}, because {why}.")
    print(f"Cases: {count}. Rounds per measurement: {rounds}, smallest kept.")
    print()
    print(f"{'side':<16}{'instructions':>16}{'cycles':>16}{'ins per cycle':>16}{'repeats':>16}")
    for row in rows:
        cycles = row.get("cycles_per_iteration")
        ratio = row.get("instructions_per_cycle")
        low, high = row["repeats"]
        print(
            f"{row['side']:<16}"
            f"{row['instructions_per_iteration']:>16,.0f}"
            f"{format(cycles, ',.0f') if cycles is not None else 'not counted':>16}"
            f"{format(ratio, '.2f') if ratio is not None else 'not quoted':>16}"
            f"{f'{low} to {high}':>16}"
        )
    print()
    worst_i = max(row["instructions_spread"] for row in rows)
    print(f"Widest instruction spread across rounds: {worst_i * 100:.2f} percent.")
    cycle_spreads = [row["cycles_spread"] for row in rows if row.get("cycles_spread") is not None]
    if cycle_spreads:
        print(f"Widest cycle spread across rounds: {max(cycle_spreads) * 100:.2f} percent.")
    print()
    print(
        "A guest that runs more instructions than the host is doing more work. A guest that runs "
        "the same instructions at a lower instructions per cycle is doing the same work worse. "
        "Those need different fixes, which is why this is measured rather than inferred from a "
        "duration."
    )
    if not steady and tool == "perf":
        print()
        print(
            "Instructions per cycle is left out above. The cycle counts moved between rounds by "
            "more than the sandbox gap being measured, which is this machine having other work on "
            "it rather than anything about the code, and a ratio built on that would be a number "
            "about the neighbours. The instruction counts are unaffected, because the same code "
            "retires the same instructions however busy the machine is."
        )
    if tool == "callgrind":
        print()
        print(
            "callgrind simulates rather than measures, so the instruction counts are exact and "
            "repeatable and there is nothing here about cycles. Anything about how well the "
            "machine issued these instructions needs a machine that will let a process read its "
            "counters."
        )


if __name__ == "__main__":
    main()
