// Produces the BtrBlocks conformance corpus by running the reference implementation.
//
// Every fixture in `fixtures/` is written by this program and by nothing else. It builds a column
// of values, hands them to the reference compressor, writes what came back, then reads that back
// through the reference reader and writes the answer. The bytes it writes are the question and the
// answer, and `iris-btr` is graded on producing the second from the first.
//
// The point of doing it this way is that the reference decides what the fixture holds. A corpus
// assembled by writing bytes we believe to be BtrBlocks would grade our reader against our own
// reading of the paper, which is the mistake this file exists to avoid.
//
// # What the answer is written as
//
// Not the buffer the reference decompressor filled. That buffer is not comparable across two
// processes: strings can come back as pointers into the compressed input rather than as bytes, and
// the slots belonging to null rows are left holding whatever was in the allocation. So the answer
// is written in a canonical form, defined here and nowhere else:
//
//   integers   `tuple_count` little endian `int32`
//   doubles    `tuple_count` little endian `float64`
//   strings    `tuple_count + 1` little endian `uint32` offsets, then the bytes they point into,
//              with the first offset counted from the start of the offset array, which is the
//              layout the reference uses for a string column
//   nullmap    `tuple_count` bytes, one per row, 1 for present and 0 for null
//
// A null row's value slot is zeroed. The reference leaves it undefined, and comparing undefined
// bytes would be comparing the allocator rather than the decoder.
//
// # Determinism
//
// Every value comes from a seeded `mt19937` and every seed is written down below, so two runs of
// this program produce the same bytes. That is what makes the digests in the manifest worth
// committing. It is not a claim that a different version of the reference produces the same bytes,
// which is why the manifest records the commit it was built against.

#include <cmath>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <random>
#include <stdexcept>
#include <string>
#include <vector>

#include "btrblocks.hpp"
#include "compression/BtrReader.hpp"
#include "compression/Datablock.hpp"
#include "extern/RoaringBitmap.hpp"
#include "storage/Chunk.hpp"
#include "storage/StringArrayViewer.hpp"
#include "storage/StringPointerArrayViewer.hpp"

using btrblocks::BITMAP;
using btrblocks::BtrReader;
using btrblocks::ColumnPart;
using btrblocks::ColumnType;
using btrblocks::Datablock;
using btrblocks::InputChunk;

namespace {

// How many rows a fixture holds. Small enough that the whole corpus is a megabyte and can live in
// the repository, large enough that every scheme has something to work with. The reference picks a
// scheme by sampling, and a column of a few hundred values is not a column it would ever see.
constexpr uint32_t kRows = 8192;

// Which rows are null in the `some` variant. Seven is coprime with everything the schemes group by,
// so the nulls do not line up with a run, a block or a word.
constexpr uint32_t kNullEvery = 7;

/// How a case decides which rows are present.
enum class Nulls { None, Some, All };

/// One row's presence under `nulls`.
BITMAP present(Nulls nulls, uint32_t row) {
  switch (nulls) {
    case Nulls::None:
      return 1;
    case Nulls::All:
      return 0;
    case Nulls::Some:
      return (row % kNullEvery) == 0 ? 0 : 1;
  }
  return 1;
}

/// The suffix a null variant puts on a case name.
const char* suffix(Nulls nulls) {
  switch (nulls) {
    case Nulls::None:
      return "";
    case Nulls::Some:
      return "-some-null";
    case Nulls::All:
      return "-all-null";
  }
  return "";
}

/// Writes `bytes` to `path`, replacing whatever was there.
void write_file(const std::filesystem::path& path, const void* bytes, size_t len) {
  std::ofstream out(path, std::ios::binary | std::ios::trunc);
  out.write(reinterpret_cast<const char*>(bytes), static_cast<std::streamsize>(len));
  out.flush();
  if (out.fail()) {
    throw std::runtime_error("writing " + path.string());
  }
}

/// Reads a whole file.
std::vector<uint8_t> read_file(const std::filesystem::path& path) {
  std::ifstream in(path, std::ios::binary | std::ios::ate);
  if (!in) {
    throw std::runtime_error("opening " + path.string());
  }
  auto len = static_cast<size_t>(in.tellg());
  std::vector<uint8_t> bytes(len);
  in.seekg(0);
  in.read(reinterpret_cast<char*>(bytes.data()), static_cast<std::streamsize>(len));
  return bytes;
}

/// A case that lets the reference pick the scheme rather than naming one.
constexpr int kAuto = -1;

/// One case: a column of values, a null variant, and the scheme to compress it with.
struct Case {
  std::string name;
  ColumnType type;
  Nulls nulls;
  // The scheme code to force, or `kAuto`. Forcing is what makes the corpus cover a scheme rather
  // than hope for it. The picker is free below the top level, so a forced DICT still gets whatever
  // the reference wants for the codes underneath it, which is the cascade a reader has to walk.
  int force;
  // Set for integers and doubles. The string cases fill `text` instead.
  std::vector<int32_t> ints;
  std::vector<double> doubles;
  std::vector<std::string> text;
};

/// The reference's own layout for a string column: offsets from the start of the offset array,
/// then the bytes, with one extra offset on the end so that every length is a subtraction.
std::vector<uint8_t> string_layout(const std::vector<std::string>& values) {
  const auto rows = static_cast<uint32_t>(values.size());
  const uint32_t header = (rows + 1) * static_cast<uint32_t>(sizeof(uint32_t));
  uint32_t total = header;
  for (const auto& value : values) {
    total += static_cast<uint32_t>(value.size());
  }

  std::vector<uint8_t> bytes(total);
  auto* offsets = reinterpret_cast<uint32_t*>(bytes.data());
  uint32_t at = header;
  for (uint32_t row = 0; row != rows; ++row) {
    offsets[row] = at;
    std::memcpy(bytes.data() + at, values[row].data(), values[row].size());
    at += static_cast<uint32_t>(values[row].size());
  }
  offsets[rows] = at;
  return bytes;
}

/// Builds the reference's input for a case.
InputChunk input_for(const Case& one) {
  auto nullmap = std::unique_ptr<BITMAP[]>(new BITMAP[kRows]);
  for (uint32_t row = 0; row != kRows; ++row) {
    nullmap[row] = present(one.nulls, row);
  }

  switch (one.type) {
    case ColumnType::INTEGER: {
      const auto size = static_cast<btrblocks::SIZE>(kRows * sizeof(int32_t));
      auto data = std::unique_ptr<uint8_t[]>(new uint8_t[size]);
      std::memcpy(data.get(), one.ints.data(), size);
      return {std::move(data), std::move(nullmap), ColumnType::INTEGER, kRows, size};
    }
    case ColumnType::DOUBLE: {
      const auto size = static_cast<btrblocks::SIZE>(kRows * sizeof(double));
      auto data = std::unique_ptr<uint8_t[]>(new uint8_t[size]);
      std::memcpy(data.get(), one.doubles.data(), size);
      return {std::move(data), std::move(nullmap), ColumnType::DOUBLE, kRows, size};
    }
    default: {
      auto laid_out = string_layout(one.text);
      const auto size = static_cast<btrblocks::SIZE>(laid_out.size());
      auto data = std::unique_ptr<uint8_t[]>(new uint8_t[size]);
      std::memcpy(data.get(), laid_out.data(), laid_out.size());
      return {std::move(data), std::move(nullmap), ColumnType::STRING, kRows, size};
    }
  }
}

/// The canonical answer for a case, zeroed where the nullmap says null.
std::vector<uint8_t> answer_for(const Case& one,
                                const std::vector<uint8_t>& decompressed,
                                bool requires_copy,
                                const std::vector<BITMAP>& nullmap) {
  switch (one.type) {
    case ColumnType::INTEGER: {
      std::vector<uint8_t> answer(kRows * sizeof(int32_t), 0);
      for (uint32_t row = 0; row != kRows; ++row) {
        if (nullmap[row] == 0) {
          continue;
        }
        std::memcpy(answer.data() + row * sizeof(int32_t),
                    decompressed.data() + row * sizeof(int32_t), sizeof(int32_t));
      }
      return answer;
    }
    case ColumnType::DOUBLE: {
      std::vector<uint8_t> answer(kRows * sizeof(double), 0);
      for (uint32_t row = 0; row != kRows; ++row) {
        if (nullmap[row] == 0) {
          continue;
        }
        std::memcpy(answer.data() + row * sizeof(double), decompressed.data() + row * sizeof(double),
                    sizeof(double));
      }
      return answer;
    }
    default: {
      // A string column can come back either as bytes or as pointers into the compressed input,
      // and which one it is depends on the scheme. Both are read here through the viewer that
      // matches, and what is written out is the same layout either way.
      std::vector<std::string> values(kRows);
      for (uint32_t row = 0; row != kRows; ++row) {
        if (nullmap[row] == 0) {
          continue;
        }
        if (requires_copy) {
          btrblocks::StringPointerArrayViewer viewer(decompressed.data());
          values[row] = std::string(viewer(row));
        } else {
          values[row] = std::string(btrblocks::StringArrayViewer::get(decompressed.data(), row));
        }
      }
      return string_layout(values);
    }
  }
}

/// The corpus.
///
/// One case per scheme per type, with the data shaped to suit the scheme and the scheme named
/// rather than left to the picker. Naming it is the point. The reference chooses by sampling the
/// column, so a corpus that let it choose would cover whichever schemes it happened to prefer, and
/// would quietly stop covering one the day the picker changed its mind. Here a case called
/// `int-bp` is bit packed because it was asked to be, and the manifest still records what came out
/// so that a case which stopped being what it says is a visible diff.
///
/// The choice is only forced at the top level. Whatever the scheme puts underneath it, the codes of
/// a dictionary or the offsets of an FSST column, is picked normally, so the cascades in the corpus
/// are the ones the reference would really produce.
std::vector<Case> cases() {
  std::vector<Case> all;

  auto ints = [](const char* name, Nulls nulls, int force, auto make) {
    Case one;
    one.name = std::string(name) + suffix(nulls);
    one.type = ColumnType::INTEGER;
    one.nulls = nulls;
    one.force = force;
    one.ints.resize(kRows);
    make(one.ints);
    return one;
  };
  auto doubles = [](const char* name, Nulls nulls, int force, auto make) {
    Case one;
    one.name = std::string(name) + suffix(nulls);
    one.type = ColumnType::DOUBLE;
    one.nulls = nulls;
    one.force = force;
    one.doubles.resize(kRows);
    make(one.doubles);
    return one;
  };
  auto strings = [](const char* name, Nulls nulls, int force, auto make) {
    Case one;
    one.name = std::string(name) + suffix(nulls);
    one.type = ColumnType::STRING;
    one.nulls = nulls;
    one.force = force;
    one.text.resize(kRows);
    make(one.text);
    return one;
  };

  auto one_value_ints = [](std::vector<int32_t>& out) {
    for (auto& value : out) {
      value = 42;
    }
  };
  auto run_ints = [](std::vector<int32_t>& out) {
    std::mt19937 gen(101);
    for (size_t at = 0; at < out.size();) {
      auto value = static_cast<int32_t>(gen() % 4095);
      for (size_t run = 0; run != 40 && at < out.size(); ++run, ++at) {
        out[at] = value;
      }
    }
  };
  auto dict_ints = [](std::vector<int32_t>& out) {
    std::mt19937 gen(102);
    std::vector<int32_t> alphabet(200);
    for (auto& value : alphabet) {
      value = static_cast<int32_t>(gen());
    }
    for (auto& value : out) {
      value = alphabet[gen() % alphabet.size()];
    }
  };
  auto tight_ints = [](std::vector<int32_t>& out) {
    std::mt19937 gen(103);
    for (auto& value : out) {
      value = 1000 + static_cast<int32_t>(gen() % 300);
    }
  };
  auto outlier_ints = [](std::vector<int32_t>& out) {
    std::mt19937 gen(104);
    for (auto& value : out) {
      value = (gen() % 100 == 0) ? static_cast<int32_t>(gen())
                                 : static_cast<int32_t>(gen() % 256);
    }
  };
  auto random_ints = [](std::vector<int32_t>& out) {
    std::mt19937 gen(105);
    for (auto& value : out) {
      value = static_cast<int32_t>(gen());
    }
  };

  auto one_value_doubles = [](std::vector<double>& out) {
    for (auto& value : out) {
      value = 3.5;
    }
  };
  auto run_doubles = [](std::vector<double>& out) {
    std::mt19937 gen(201);
    for (size_t at = 0; at < out.size();) {
      auto value = static_cast<double>(gen() % 4095);
      for (size_t run = 0; run != 40 && at < out.size(); ++run, ++at) {
        out[at] = value;
      }
    }
  };
  auto dict_doubles = [](std::vector<double>& out) {
    std::mt19937 gen(202);
    std::vector<double> alphabet(200);
    for (auto& value : alphabet) {
      value = static_cast<double>(gen()) / 7.0;
    }
    for (auto& value : out) {
      value = alphabet[gen() % alphabet.size()];
    }
  };
  auto decimal_doubles = [](std::vector<double>& out) {
    std::mt19937 gen(203);
    for (auto& value : out) {
      value = static_cast<double>(gen() % 1000000) / 100.0;
    }
  };
  auto frequent_doubles = [](std::vector<double>& out) {
    // One value on four rows in five, the rest spread out. That is the shape frequency compression
    // is for: store the common value once with a bitmap of where it is not, and the exceptions
    // beside it.
    std::mt19937 gen(205);
    for (auto& value : out) {
      value = (gen() % 5 == 0) ? static_cast<double>(gen() % 100000) / 8.0 : 12.25;
    }
  };
  auto random_doubles = [](std::vector<double>& out) {
    std::mt19937_64 gen(204);
    for (auto& value : out) {
      auto bits = gen();
      std::memcpy(&value, &bits, sizeof(double));
      if (!std::isfinite(value)) {
        value = static_cast<double>(bits % 1000) + 0.5;
      }
    }
  };

  auto one_value_strings = [](std::vector<std::string>& out) {
    for (auto& value : out) {
      value = "the same string every time";
    }
  };
  auto dict_strings = [](std::vector<std::string>& out) {
    std::mt19937 gen(301);
    std::vector<std::string> alphabet;
    for (int at = 0; at != 100; ++at) {
      alphabet.push_back("value-" + std::to_string(at));
    }
    for (auto& value : out) {
      value = alphabet[gen() % alphabet.size()];
    }
  };
  auto text_strings = [](std::vector<std::string>& out) {
    std::mt19937 gen(302);
    const char* words[] = {"the",  "quick",   "brown", "fox",     "jumps", "over",
                           "lazy", "dog",     "and",   "then",    "runs",  "away",
                           "into", "forest",  "where", "nobody",  "can",   "find",
                           "it",   "anymore", "which", "is",      "fine",  "really"};
    const auto count = sizeof(words) / sizeof(words[0]);
    for (auto& value : out) {
      value.clear();
      auto length = 3 + gen() % 6;
      for (uint32_t word = 0; word != length; ++word) {
        if (word != 0) {
          value += ' ';
        }
        value += words[gen() % count];
      }
    }
  };
  auto random_strings = [](std::vector<std::string>& out) {
    std::mt19937 gen(303);
    for (auto& value : out) {
      auto length = 8 + gen() % 24;
      value.resize(length);
      for (auto& character : value) {
        // Printable only. The reference reads a string column as bytes and does not care, but a
        // fixture a person can look at when it disagrees is worth the narrower alphabet.
        character = static_cast<char>(33 + gen() % 94);
      }
    }
  };

  using Ints = btrblocks::IntegerSchemeType;
  using Dbls = btrblocks::DoubleSchemeType;
  using Strs = btrblocks::StringSchemeType;
  auto code = [](auto scheme) { return static_cast<int>(scheme); };

  all.push_back(ints("int-uncompressed", Nulls::None, code(Ints::UNCOMPRESSED), random_ints));
  all.push_back(ints("int-one-value", Nulls::None, code(Ints::ONE_VALUE), one_value_ints));
  all.push_back(ints("int-dict", Nulls::None, code(Ints::DICT), dict_ints));
  all.push_back(ints("int-rle", Nulls::None, code(Ints::RLE), run_ints));
  all.push_back(ints("int-pfor", Nulls::None, code(Ints::PFOR), outlier_ints));
  all.push_back(ints("int-bp", Nulls::None, code(Ints::BP), tight_ints));

  all.push_back(doubles("dbl-uncompressed", Nulls::None, code(Dbls::UNCOMPRESSED), random_doubles));
  all.push_back(doubles("dbl-one-value", Nulls::None, code(Dbls::ONE_VALUE), one_value_doubles));
  all.push_back(doubles("dbl-dict", Nulls::None, code(Dbls::DICT), dict_doubles));
  all.push_back(doubles("dbl-rle", Nulls::None, code(Dbls::RLE), run_doubles));
  all.push_back(doubles("dbl-frequency", Nulls::None, code(Dbls::FREQUENCY), frequent_doubles));
  all.push_back(
      doubles("dbl-pseudodecimal", Nulls::None, code(Dbls::PSEUDODECIMAL), decimal_doubles));

  all.push_back(strings("str-uncompressed", Nulls::None, code(Strs::UNCOMPRESSED), random_strings));
  all.push_back(strings("str-one-value", Nulls::None, code(Strs::ONE_VALUE), one_value_strings));
  all.push_back(strings("str-dict", Nulls::None, code(Strs::DICT), dict_strings));
  all.push_back(strings("str-fsst", Nulls::None, code(Strs::FSST), text_strings));

  // A scattered nullmap on one scheme per type, which is where a decoder that reads the values and
  // the nullmap independently of each other gets caught.
  all.push_back(ints("int-dict", Nulls::Some, code(Ints::DICT), dict_ints));
  all.push_back(
      doubles("dbl-pseudodecimal", Nulls::Some, code(Dbls::PSEUDODECIMAL), decimal_doubles));
  all.push_back(strings("str-fsst", Nulls::Some, code(Strs::FSST), text_strings));

  // A column with nothing in it. The reference short circuits this to ONE_VALUE before it looks at
  // any forced scheme, so these are left on auto and named for the shape rather than the scheme.
  all.push_back(ints("int", Nulls::All, kAuto, tight_ints));
  all.push_back(doubles("dbl", Nulls::All, kAuto, run_doubles));
  all.push_back(strings("str", Nulls::All, kAuto, dict_strings));

  return all;
}

/// A scheme description on one line.
///
/// The reference describes a cascade over several lines with a tab on each, which reads well on a
/// terminal and does not belong in a tab separated file. The nesting survives as the arrows, which
/// is the part that says what a reader has to implement.
std::string one_line(std::string description) {
  std::string flat;
  bool space = false;
  for (char character : description) {
    if (character == '\n' || character == '\t' || character == ' ') {
      space = !flat.empty();
      continue;
    }
    if (space) {
      flat += ' ';
      space = false;
    }
    flat += character;
  }
  return flat;
}

/// The name the manifest gives a column type.
const char* type_name(ColumnType type) {
  switch (type) {
    case ColumnType::INTEGER:
      return "integer";
    case ColumnType::DOUBLE:
      return "double";
    case ColumnType::STRING:
      return "string";
    default:
      return "unsupported";
  }
}

/// Tells the reference which scheme to use for the next column it compresses.
///
/// The reference reads this once and puts it back to automatic, so it has to be set again before
/// every column rather than once at startup.
void force(const Case& one) {
  auto& config = btrblocks::BtrBlocksConfig::get();
  const auto automatic = static_cast<int>(btrblocks::autoScheme());
  config.integers.override_scheme = static_cast<btrblocks::IntegerSchemeType>(automatic);
  config.doubles.override_scheme = static_cast<btrblocks::DoubleSchemeType>(automatic);
  config.strings.override_scheme = static_cast<btrblocks::StringSchemeType>(automatic);
  if (one.force == kAuto) {
    return;
  }
  switch (one.type) {
    case ColumnType::INTEGER:
      config.integers.override_scheme = static_cast<btrblocks::IntegerSchemeType>(one.force);
      break;
    case ColumnType::DOUBLE:
      config.doubles.override_scheme = static_cast<btrblocks::DoubleSchemeType>(one.force);
      break;
    default:
      config.strings.override_scheme = static_cast<btrblocks::StringSchemeType>(one.force);
      break;
  }
}

/// Checks that what came back out is what went in.
///
/// Nothing in the reference stops a scheme being forced onto data it was not meant for, and a
/// scheme that mangles such a column would still produce a fixture, which would then be committed
/// as the answer our reader is graded against. That is the one way this program can be confidently,
/// silently wrong, so it is the one thing it checks.
void check(const Case& one, const std::vector<uint8_t>& answer, const std::vector<BITMAP>& nullmap) {
  const auto complain = [&one](const std::string& what) {
    throw std::runtime_error("case " + one.name + ": " + what);
  };

  for (uint32_t row = 0; row != kRows; ++row) {
    const bool expected = present(one.nulls, row) != 0;
    if ((nullmap[row] != 0) != expected) {
      complain("row " + std::to_string(row) + " came back with the wrong presence");
    }
    if (!expected) {
      continue;
    }
    switch (one.type) {
      case ColumnType::INTEGER: {
        int32_t got = 0;
        std::memcpy(&got, answer.data() + row * sizeof(int32_t), sizeof(int32_t));
        if (got != one.ints[row]) {
          complain("row " + std::to_string(row) + " came back as " + std::to_string(got));
        }
        break;
      }
      case ColumnType::DOUBLE: {
        double got = 0;
        std::memcpy(&got, answer.data() + row * sizeof(double), sizeof(double));
        // Bit for bit. A scheme that took a double apart and put it back together a hair off is
        // exactly what this is looking for, and a tolerance would hide it.
        if (std::memcmp(&got, &one.doubles[row], sizeof(double)) != 0) {
          complain("row " + std::to_string(row) + " came back as " + std::to_string(got));
        }
        break;
      }
      default: {
        const auto* offsets = reinterpret_cast<const uint32_t*>(answer.data());
        const std::string got(reinterpret_cast<const char*>(answer.data()) + offsets[row],
                              offsets[row + 1] - offsets[row]);
        if (got != one.text[row]) {
          complain("row " + std::to_string(row) + " came back as " + got);
        }
        break;
      }
    }
  }
}

}  // namespace

int main(int argc, char** argv) {
  if (argc != 3) {
    std::cerr << "usage: generate <output directory> <reference commit>\n";
    return 2;
  }
  const std::filesystem::path out(argv[1]);
  const std::string commit(argv[2]);
  std::filesystem::create_directories(out);

  btrblocks::BtrBlocksConfig::configure([](btrblocks::BtrBlocksConfig& config) {
    // The defaults, said out loud, because a corpus generated under a configuration nobody wrote
    // down is a corpus nobody can regenerate. The cascade depth in particular decides whether a
    // scheme's own output is compressed again, which is most of what a reader has to handle.
    config.block_size = 65536;
    config.integers.max_cascade_depth = 3;
    config.doubles.max_cascade_depth = 3;
    config.strings.max_cascade_depth = 3;

    // Sampling, which is the default and is also the only selection mode that honours a forced
    // scheme. Trying every scheme instead would remove one source of run to run variation, and it
    // would also take the choice away from us and hand it to whichever scheme happens to produce
    // the smallest output, which on this data is never bit packing. A corpus that cannot cover BP
    // is worse than a corpus that has to be committed rather than regenerated.
    config.scheme_selection = btrblocks::SchemeSelection::SAMPLE;
  });

  std::ofstream manifest(out / "manifest.txt", std::ios::trunc);
  manifest << "# Written by conformance/btrblocks/generate.cpp against the reference at\n";
  manifest << "# " << commit << ". One line per case: name, type, rows, scheme.\n";

  for (const auto& one : cases()) {
    auto input = input_for(one);
    force(one);
    auto compressed = Datablock::compress(input);

    ColumnPart part;
    part.addCompressedChunk(std::move(compressed));
    const auto path = out / (one.name + ".btr");
    part.writeToDisk(path.string());

    auto raw = read_file(path);
    BtrReader reader(raw.data());

    std::vector<uint8_t> decompressed;
    const bool requires_copy = reader.readColumn(decompressed, 0);

    // The reference's own way of turning its bitmap back into one byte a row, rather than a loop
    // here asking `test` for each one. Whether a row is present is the reference's answer to give.
    auto nullmap = reader.getBitmap(0)->writeBITMAP();
    if (nullmap.size() != kRows) {
      throw std::runtime_error("the reference returned a nullmap of the wrong length");
    }

    auto answer = answer_for(one, decompressed, requires_copy, nullmap);
    check(one, answer, nullmap);
    write_file(out / (one.name + ".out"), answer.data(), answer.size());
    write_file(out / (one.name + ".null"), nullmap.data(), nullmap.size());

    const auto scheme = one_line(reader.getSchemeDescription(0));
    manifest << one.name << '\t' << type_name(one.type) << '\t' << kRows << '\t' << scheme << '\n';
    std::cout << one.name << '\t' << scheme << '\n';
  }

  manifest.flush();
  return 0;
}
