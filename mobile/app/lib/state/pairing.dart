import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/link.dart';
import 'prefs.dart';

/// What `femmed pair` prints as a QR code:
/// `femme://pair?host=<board host>&tls=1&name=<box name>&token=<token>`.
class PairLink {
  const PairLink({
    required this.host,
    required this.tls,
    required this.name,
    required this.token,
  });

  /// The board host as the link carries it (`box.example.com`, maybe with a port).
  final String host;
  final bool tls;

  /// The box's name, for showing.
  final String name;
  final String token;

  /// The address [Prefs] keeps: `wss://host` (or `ws://host` when `tls=0`); the link adds
  /// `/femme/ws`.
  String get address => '${tls ? 'wss' : 'ws'}://$host';

  /// Parses a scanned or pasted link; null when it is not a pairing link or lacks host or token.
  static PairLink? parse(String text) {
    final t = text.trim();
    if (!t.toLowerCase().startsWith('femme:')) return null;
    final Uri u;
    try {
      u = Uri.parse(t);
    } on FormatException {
      return null;
    }
    final what = u.host.isNotEmpty ? u.host : u.path.replaceAll('/', '');
    if (what.toLowerCase() != 'pair') return null;
    final q = u.queryParameters;
    final host = (q['host'] ?? '').trim();
    final token = (q['token'] ?? '').trim();
    if (host.isEmpty || token.isEmpty) return null;
    final tls = !const {'0', 'false', 'no'}.contains(q['tls']?.toLowerCase());
    final name = (q['name'] ?? '').trim();
    return PairLink(
      host: host,
      tls: tls,
      name: name.isEmpty ? host : name,
      token: token,
    );
  }
}

/// The pairing's extras the shared [Prefs] does not hold: the box's name and when it was paired.
abstract final class PairInfo {
  static const _kName = 'pair.name';
  static const _kAt = 'pair.at';

  static SharedPreferences? _sp;

  static Future<void> load() async =>
      _sp ??= await SharedPreferences.getInstance();

  static String? get boxName => _sp?.getString(_kName);
  static DateTime? get pairedAt {
    final ms = _sp?.getInt(_kAt);
    return ms == null ? null : DateTime.fromMillisecondsSinceEpoch(ms);
  }

  /// Saves [link] (host + token in [prefs], which makes the app connect) and its name.
  static Future<void> save(Prefs prefs, PairLink link) =>
      saveManual(prefs, link.address, link.token, name: link.name);

  static Future<void> saveManual(
    Prefs prefs,
    String host,
    String token, {
    String? name,
  }) async {
    await load();
    if (name != null && name.isNotEmpty) {
      await _sp!.setString(_kName, name);
    } else {
      await _sp!.remove(_kName);
    }
    await _sp!.setInt(_kAt, DateTime.now().millisecondsSinceEpoch);
    await prefs.setPairing(host.trim(), token.trim());
  }

  static Future<void> forget(Prefs prefs) async {
    await load();
    await _sp!.remove(_kName);
    await _sp!.remove(_kAt);
    await prefs.forget();
  }
}

/// When the link last came online (null while it is not): the Me tab's "connected since".
class LinkClock {
  LinkClock(this._state) {
    _state.addListener(_on);
    _on();
  }

  final ValueListenable<LinkState> _state;
  static DateTime? onlineSince;

  void _on() {
    if (!_state.value.online) {
      onlineSince = null;
    } else {
      onlineSince ??= DateTime.now();
    }
  }

  void dispose() => _state.removeListener(_on);
}
