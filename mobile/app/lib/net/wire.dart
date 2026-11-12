/// Helpers for serde_json's encoding of femme-proto types.
library;

/// A decoded JSON object.
typedef Json = Map<String, dynamic>;

/// Splits an externally tagged enum value: a unit variant is `"Name"`, any other variant is
/// `{"Name": value}`. Returns the name and the value (null for a unit variant).
(String, Object?) tagOf(Object? json) {
  if (json is String) return (json, null);
  if (json is Map && json.length == 1) {
    final e = json.entries.first;
    return (e.key as String, e.value);
  }
  throw FormatException('not an externally tagged enum: $json');
}

/// An enum by its serde name, or [fallback] for a name this build does not know (a newer server).
T enumByName<T extends Enum>(List<T> values, Object? name, T fallback) {
  for (final v in values) {
    if (v.name == name) return v;
  }
  return fallback;
}

/// Dart enum names are lowerCamel; femme-proto's are UpperCamel.
String wireName(Enum e) => e.name[0].toUpperCase() + e.name.substring(1);

T wireEnum<T extends Enum>(List<T> values, Object? name, T fallback) {
  if (name is! String || name.isEmpty) return fallback;
  return enumByName(
    values,
    name[0].toLowerCase() + name.substring(1),
    fallback,
  );
}

/// bitflags 2 serializes human-readably as flag names joined by `" | "`, `""` for none. All
/// bitflags on the wire (`Mods`, `Attrs`, `Modes`) go through these two so the encoding can be
/// switched in one place.
Set<String> flagsFromJson(Object? json) {
  if (json is String) {
    return json
        .split('|')
        .map((s) => s.trim())
        .where((s) => s.isNotEmpty)
        .toSet();
  }
  if (json == null) return {};
  throw FormatException('bitflags: $json');
}

Object flagsToJson(Iterable<String> flags) => flags.join(' | ');

/// `[u8; 32]` and `Vec<u8>`: arrays of ints.
List<int> bytesFromJson(Object? json) => (json as List).cast<int>();

List<int>? bytesOrNull(Object? json) =>
    json == null ? null : bytesFromJson(json);

List<T> listOf<T>(Object? json, T Function(Json) f) =>
    (json as List).map((e) => f(e as Json)).toList();
