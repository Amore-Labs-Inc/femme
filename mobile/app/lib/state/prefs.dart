import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/ids.dart';
import 'push.dart';

/// Where the store keeps the prompts not confirmed yet (see `Store`).
abstract interface class OutboxKeeper {
  String? get outboxJson;
  void saveOutbox(String? json);
}

enum ThemePref { system, light, dark }

/// Settings kept on the phone: where the box is, the device token, this install's
/// `client_instance`, and the theme.
class Prefs extends ChangeNotifier implements OutboxKeeper, PushKeeper {
  Prefs._(this._sp, this.clientInstance);

  static const _kHost = 'host';
  static const _kToken = 'token';
  static const _kInstance = 'client_instance';
  static const _kTheme = 'theme';
  static const _kOutbox = 'outbox';
  static const _kPushTopic = 'push.topic';
  static const _kPushOn = 'push.on';
  static const _kPushRules = 'push.rules';

  /// The instance [load] made last. `Store()` keeps its outbox here when it is not handed a
  /// keeper; null in tests that never load prefs (the store then keeps it in memory only).
  static Prefs? current;

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
    return current = Prefs._(sp, instance);
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

  /// The outbox as JSON (a list of entries), null when nothing is waiting.
  @override
  String? get outboxJson => _sp.getString(_kOutbox);

  /// Written on every outbox change; shared_preferences updates its cache at once and writes to
  /// disk in the background. Listeners are not told: nothing on screen reads it from here.
  @override
  void saveOutbox(String? json) {
    (json == null ? _sp.remove(_kOutbox) : _sp.setString(_kOutbox, json))
        .ignore();
  }

  Future<void> setTheme(ThemePref t) async {
    await _sp.setString(_kTheme, t.name);
    notifyListeners();
  }

  @override
  String? get pushTopic => _sp.getString(_kPushTopic);
  @override
  bool get pushOn => _sp.getBool(_kPushOn) ?? false;
  @override
  String? get pushRulesJson => _sp.getString(_kPushRules);

  @override
  Future<void> setPush({String? topic, bool? on, String? rulesJson}) async {
    if (topic != null) await _sp.setString(_kPushTopic, topic);
    if (on != null) await _sp.setBool(_kPushOn, on);
    if (rulesJson != null) await _sp.setString(_kPushRules, rulesJson);
    notifyListeners();
  }
}
