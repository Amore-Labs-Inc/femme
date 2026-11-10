import 'dart:math';

/// ULIDs as femme-proto carries them in JSON: 26 Crockford base32 characters, a 48-bit unix-ms
/// timestamp followed by 80 random bits.
abstract final class Ulid {
  static const _alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  static final _rand = Random.secure();

  /// The nil id (`Id(0)`), which the server uses for a `Resp` that answers no request.
  static const nil = '00000000000000000000000000';

  static String next([DateTime? at]) {
    var ms = (at ?? DateTime.now()).millisecondsSinceEpoch;
    final out = List<String>.filled(26, '0');
    for (var i = 9; i >= 0; i--) {
      out[i] = _alphabet[ms & 31];
      ms >>= 5;
    }
    for (var i = 10; i < 26; i++) {
      out[i] = _alphabet[_rand.nextInt(32)];
    }
    return out.join();
  }

  /// The unix-ms time a ULID was made at.
  static int timeOf(String ulid) {
    var ms = 0;
    for (var i = 0; i < 10; i++) {
      ms = (ms << 5) | _alphabet.indexOf(ulid[i].toUpperCase());
    }
    return ms;
  }

  static bool isValid(String s) =>
      s.length == 26 &&
      s.toUpperCase().split('').every(_alphabet.contains) &&
      '01234567'.contains(s[0]);
}

/// A random 52-bit integer: `client_instance` is a u128 on the wire, but JSON numbers past 2^53
/// lose precision in many decoders.
int randomClientInstance() {
  final r = Random.secure();
  return (r.nextInt(1 << 26) << 26) | r.nextInt(1 << 26);
}
