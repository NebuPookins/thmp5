#include <taglib/fileref.h>
#include <taglib/tpropertymap.h>
#include <taglib/tag.h>

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <cstdlib>
#include <exception>
#include <iomanip>
#include <iostream>
#include <optional>
#include <sstream>
#include <string>
#include <string_view>
#include <vector>

namespace {

struct Request {
  std::string path;
};

void skip_ws(std::string_view input, std::size_t& pos) {
  while (pos < input.size() &&
         std::isspace(static_cast<unsigned char>(input[pos])) != 0) {
    ++pos;
  }
}

std::optional<std::string> parse_json_string(std::string_view input,
                                             std::size_t& pos) {
  if (pos >= input.size() || input[pos] != '"') {
    return std::nullopt;
  }
  ++pos;

  std::string out;
  while (pos < input.size()) {
    const char c = input[pos++];
    if (c == '"') {
      return out;
    }
    if (c != '\\') {
      out.push_back(c);
      continue;
    }
    if (pos >= input.size()) {
      return std::nullopt;
    }
    const char esc = input[pos++];
    switch (esc) {
      case '"':
      case '\\':
      case '/':
        out.push_back(esc);
        break;
      case 'b':
        out.push_back('\b');
        break;
      case 'f':
        out.push_back('\f');
        break;
      case 'n':
        out.push_back('\n');
        break;
      case 'r':
        out.push_back('\r');
        break;
      case 't':
        out.push_back('\t');
        break;
      case 'u': {
        if (pos + 4 > input.size()) {
          return std::nullopt;
        }
        const auto hex = input.substr(pos, 4);
        pos += 4;
        unsigned int code = 0;
        std::istringstream parser{std::string(hex)};
        parser >> std::hex >> code;
        if (!parser) {
          return std::nullopt;
        }
        if (code <= 0x7F) {
          out.push_back(static_cast<char>(code));
        } else if (code <= 0x7FF) {
          out.push_back(static_cast<char>(0xC0 | ((code >> 6) & 0x1F)));
          out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
        } else {
          out.push_back(static_cast<char>(0xE0 | ((code >> 12) & 0x0F)));
          out.push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
          out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
        }
        break;
      }
      default:
        return std::nullopt;
    }
  }

  return std::nullopt;
}

std::optional<Request> parse_request(std::string_view input) {
  std::size_t pos = 0;
  skip_ws(input, pos);
  if (pos >= input.size() || input[pos] != '{') {
    return std::nullopt;
  }
  ++pos;
  skip_ws(input, pos);

  const auto key = parse_json_string(input, pos);
  if (!key || *key != "path") {
    return std::nullopt;
  }
  skip_ws(input, pos);
  if (pos >= input.size() || input[pos] != ':') {
    return std::nullopt;
  }
  ++pos;
  skip_ws(input, pos);
  const auto path = parse_json_string(input, pos);
  if (!path) {
    return std::nullopt;
  }
  skip_ws(input, pos);
  if (pos >= input.size() || input[pos] != '}') {
    return std::nullopt;
  }
  ++pos;
  skip_ws(input, pos);
  if (pos != input.size()) {
    return std::nullopt;
  }
  return Request{*path};
}

std::string json_escape(std::string_view input) {
  std::ostringstream out;
  for (const unsigned char c : input) {
    switch (c) {
      case '"':
        out << "\\\"";
        break;
      case '\\':
        out << "\\\\";
        break;
      case '\b':
        out << "\\b";
        break;
      case '\f':
        out << "\\f";
        break;
      case '\n':
        out << "\\n";
        break;
      case '\r':
        out << "\\r";
        break;
      case '\t':
        out << "\\t";
        break;
      default:
        if (c < 0x20) {
          out << "\\u" << std::hex << std::setw(4) << std::setfill('0')
              << static_cast<int>(c) << std::dec << std::setfill(' ');
        } else {
          out << static_cast<char>(c);
        }
        break;
    }
  }
  return out.str();
}

std::string to_utf8(const TagLib::String& value) {
  return value.to8Bit(true);
}

std::optional<std::string> first_property(const TagLib::PropertyMap& props,
                                          const char* key) {
  const auto it = props.find(key);
  if (it == props.end() || it->second.isEmpty()) {
    return std::nullopt;
  }
  return to_utf8(it->second.front());
}

std::optional<std::string> first_present(std::initializer_list<std::optional<std::string>> values) {
  for (const auto& value : values) {
    if (value && !value->empty()) {
      return value;
    }
  }
  return std::nullopt;
}

std::optional<uint32_t> parse_u32(const std::optional<std::string>& value) {
  if (!value || value->empty()) {
    return std::nullopt;
  }
  try {
    std::size_t used = 0;
    const auto parsed = std::stoul(*value, &used, 10);
    // Accept full-string parse (e.g. "2009" -> 2009).
    if (used == value->size()) {
      return static_cast<uint32_t>(parsed);
    }
  } catch (...) {
  }
  // Try parsing just the part before '/' for "1/13" style values.
  const auto slash = value->find('/');
  if (slash != std::string::npos) {
    try {
      std::size_t used = 0;
      const auto parsed = std::stoul(value->substr(0, slash), &used, 10);
      if (used == slash) {
        return static_cast<uint32_t>(parsed);
      }
    } catch (...) {
    }
  }
  return std::nullopt;
}

/// Parse the part after '/' in "1/13" style values (e.g. track_total, disc_total).
std::optional<uint32_t> parse_second_u32(const std::optional<std::string>& value) {
  if (!value || value->empty()) {
    return std::nullopt;
  }
  const auto slash = value->find('/');
  if (slash == std::string::npos || slash + 1 >= value->size()) {
    return std::nullopt;
  }
  try {
    std::size_t used = 0;
    const auto parsed = std::stoul(value->substr(slash + 1), &used, 10);
    if (used == value->size() - slash - 1) {
      return static_cast<uint32_t>(parsed);
    }
  } catch (...) {
  }
  return std::nullopt;
}

std::string lower_ext(std::string_view path) {
  const auto slash = path.find_last_of("/\\");
  const auto dot = path.find_last_of('.');
  if (dot == std::string_view::npos || (slash != std::string_view::npos && dot < slash + 1)) {
    return "unknown";
  }
  std::string ext(path.substr(dot + 1));
  std::transform(ext.begin(), ext.end(), ext.begin(), [](unsigned char c) {
    return static_cast<char>(std::tolower(c));
  });
  return ext;
}

void print_json_string_field(std::ostream& out, const char* key,
                             const std::optional<std::string>& value) {
  out << '"' << key << "\":";
  if (value) {
    out << '"' << json_escape(*value) << '"';
  } else {
    out << "null";
  }
}

void print_json_u32_field(std::ostream& out, const char* key,
                          const std::optional<uint32_t>& value) {
  out << '"' << key << "\":";
  if (value) {
    out << *value;
  } else {
    out << "null";
  }
}

void print_json_null_field(std::ostream& out, const char* key) {
  out << '"' << key << "\":null";
}

/// Output all properties from the TagLib property map as a JSON array of {key, value} objects.
void print_all_properties(std::ostream& out, const TagLib::PropertyMap& props) {
  out << '"' << "all_tags" << "\":[";
  bool first = true;
  for (const auto& [key, values] : props) {
    if (!values.isEmpty()) {
      if (!first) out << ',';
      first = false;
      out << '{'
          << '"' << "key" << "\":\"" << json_escape(key.to8Bit(true)) << '"' << ','
          << '"' << "value" << "\":\"" << json_escape(values.front().to8Bit(true)) << '"'
          << '}';
    }
  }
  out << ']';
}

}  // namespace

int main(int argc, char** argv) {
  try {
    if (argc != 2) {
      std::cerr << "expected one JSON argument\n";
      return 2;
    }

    const auto request = parse_request(argv[1]);
    if (!request || request->path.empty()) {
      std::cerr << "invalid request JSON\n";
      return 2;
    }

    TagLib::FileRef file(request->path.c_str());
    if (file.isNull()) {
      std::cerr << "failed to open file with TagLib\n";
      return 1;
    }

    TagLib::Tag* tag = file.tag();
    const auto props = file.file() ? file.file()->properties() : TagLib::PropertyMap{};

    const auto title =
        first_property(props, "TITLE")
            .value_or(tag ? to_utf8(tag->title()) : std::string{});
    const auto artist =
        first_property(props, "ARTIST")
            .value_or(tag ? to_utf8(tag->artist()) : std::string{});
    const auto album =
        first_property(props, "ALBUM")
            .value_or(tag ? to_utf8(tag->album()) : std::string{});
    const auto genre =
        first_property(props, "GENRE")
            .value_or(tag ? to_utf8(tag->genre()) : std::string{});
    const auto comment =
        first_property(props, "COMMENT")
            .value_or(tag ? to_utf8(tag->comment()) : std::string{});

    const auto album_artist =
        first_present({first_property(props, "ALBUMARTIST"),
                       first_property(props, "ALBUM ARTIST")});

    const auto year =
        parse_u32(first_present({first_property(props, "DATE"), first_property(props, "YEAR")}));
    const auto track_number =
        parse_u32(first_present({first_property(props, "TRACKNUMBER"), first_property(props, "TRACK")}));
    const auto track_number_raw =
        first_present({first_property(props, "TRACKNUMBER"), first_property(props, "TRACK")});
    const auto track_total_val = parse_u32(
        first_present({first_property(props, "TRACKTOTAL"), first_property(props, "TOTALTRACKS")}));
    // Fall back to parsing the second component from "1/13" style TRACKNUMBER.
    const auto track_total = track_total_val ? track_total_val
        : (track_number_raw ? parse_second_u32(track_number_raw) : std::optional<uint32_t>{});
    const auto disc_number =
        parse_u32(first_present({first_property(props, "DISCNUMBER"), first_property(props, "DISC")}));

    const auto duration_ms = file.audioProperties()
                                 ? static_cast<std::uint64_t>(file.audioProperties()->lengthInMilliseconds())
                                 : 0ULL;

    std::cout << '{';
    std::cout << "\"meta\":{";
    print_json_string_field(std::cout, "title",
                            title.empty() ? std::nullopt : std::make_optional(title));
    std::cout << ',';
    print_json_string_field(std::cout, "artist",
                            artist.empty() ? std::nullopt : std::make_optional(artist));
    std::cout << ',';
    print_json_string_field(std::cout, "album_artist", album_artist);
    std::cout << ',';
    print_json_string_field(std::cout, "album",
                            album.empty() ? std::nullopt : std::make_optional(album));
    std::cout << ',';
    print_json_u32_field(std::cout, "year", year);
    std::cout << ',';
    print_json_u32_field(std::cout, "track_number", track_number);
    std::cout << ',';
    print_json_u32_field(std::cout, "track_total", track_total);
    std::cout << ',';
    print_json_u32_field(std::cout, "disc_number", disc_number);
    std::cout << ',';
    std::cout << "\"duration_ms\":" << duration_ms << ',';
    print_json_string_field(std::cout, "format",
                            std::make_optional(lower_ext(request->path)));
    std::cout << ',';
    print_json_string_field(std::cout, "genre",
                            genre.empty() ? std::nullopt : std::make_optional(genre));
    std::cout << ',';
    print_json_null_field(std::cout, "bpm");
    std::cout << ',';
    print_json_string_field(std::cout, "comment",
                            comment.empty() ? std::nullopt : std::make_optional(comment));
    std::cout << ',';
    print_json_null_field(std::cout, "replay_gain_track_db");
    std::cout << ',';
    print_json_null_field(std::cout, "replay_gain_track_peak");
    std::cout << ',';
    print_json_null_field(std::cout, "replay_gain_album_db");
    std::cout << ',';
    print_json_null_field(std::cout, "replay_gain_album_peak");
    std::cout << "},";
    print_all_properties(std::cout, props);
    std::cout << ',';
    std::cout
        << "\"warning\":\"Recovered metadata with TagLib sidecar after Lofty rejected the tags.\"";
    std::cout << '}';
    std::cout << '\n';
    return 0;
  } catch (const std::exception& error) {
    std::cerr << "taglib-helper error: " << error.what() << '\n';
    return 1;
  } catch (...) {
    std::cerr << "taglib-helper error: unknown exception\n";
    return 1;
  }
}
