import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/ids.dart';

enum ThemePref { system, light, dark }

/// Settings kept on the phone: where the box is, the device token, this install's
/// `client_instance`, and the theme.
class Prefs extends ChangeNotifier {
  Prefs._(this._sp, this.clientInstance);

  static const _kHost = 'host';
  static const _kToken = 'token';
  static const _kInstance = 'client_instance';
  static const _kTheme = 'theme';

  final SharedPreferences _sp;

  /// Picked once per install and kept: the box uses it to carry terminal input seqs across
  /// reconnects.
  final int clientInstance;

  static Future<Prefs> load() async {
    final sp = await SharedPreferences.getInstance();
    var instance = sp.getInt(_kInstance);
    if (instance == null) {
      instance = randomClientInstance();
      await sp.setInt(_kInstance, instance);
    }
    return Prefs._(sp, instance);
  }

  String? get host => _sp.getString(_kHost);
  String? get token => _sp.getString(_kToken);
  bool get paired =>
      (host?.isNotEmpty ?? false) && (token?.isNotEmpty ?? false);

  ThemePref get theme {
    final name = _sp.getString(_kTheme);
    return ThemePref.values.firstWhere(
      (t) => t.name == name,
      orElse: () => ThemePref.system,
    );
  }

  Future<void> setPairing(String host, String token) async {
    await _sp.setString(_kHost, host);
    await _sp.setString(_kToken, token);
    notifyListeners();
  }

  Future<void> forget() async {
    await _sp.remove(_kToken);
    notifyListeners();
  }

  Future<void> setTheme(ThemePref t) async {
    await _sp.setString(_kTheme, t.name);
    notifyListeners();
  }
}
