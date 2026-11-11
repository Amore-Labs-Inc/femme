import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/ids.dart';
import 'push.dart';

/// Where the store keeps the prompts not confirmed yet (see `Store`).
abstract interface class OutboxKeeper {
  String? get outboxJson;
  void saveOutbox(String? json);
}

enum ThemePref { system, light, dark }

/// Where the device token is kept outside shared_preferences: the Keychain (iOS, macOS) or the
/// Keystore-backed storage (Android). Tests hand their own.
abstract interface class TokenVault {
  Future<String?> read();

  /// Null deletes it.
  Future<void> write(String? token);

  /// The platform's vault on iOS, Android and macOS; null elsewhere (Linux desktop, tests), where
  /// the token stays in shared_preferences.
  static TokenVault? platform() {
    if (kIsWeb) return null;
    if (!(Platform.isIOS || Platform.isAndroid || Platform.isMacOS)) {
      return null;
    }
    return const _SecureVault();
  }
}

class _SecureVault implements TokenVault {
  const _SecureVault();
  static const _key = 'femme.token';

  // Readable after the first unlock, so a reconnect on wake works with the phone locked.
  static const _storage = FlutterSecureStorage(
    iOptions: IOSOptions(accessibility: KeychainAccessibility.first_unlock),
    mOptions: MacOsOptions(accessibility: KeychainAccessibility.first_unlock),
  );

  @override
  Future<String?> read() => _storage.read(key: _key);

  @override
  Future<void> write(String? token) => token == null
      ? _storage.delete(key: _key)
      : _storage.write(key: _key, value: token);
}

/// Settings kept on the phone: where the box is, the device token (in the [TokenVault] when the
/// platform has one), this install's `client_instance`, and the theme.
class Prefs extends ChangeNotifier implements OutboxKeeper, PushKeeper {
  Prefs._(this._sp, this.clientInstance, this._vault, this._token);

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

  /// Null: the token lives in shared_preferences (no vault here, or the vault failed).
  TokenVault? _vault;
  String? _token;

  /// Picked once per install and kept: the box uses it to carry terminal input seqs across
  /// reconnects.
  final int clientInstance;

  /// Reads the settings. A token an older install left in shared_preferences moves to the vault
  /// (and is removed there) on the first load that has one. [vault] defaults to
  /// [TokenVault.platform]; a vault that fails (plugin missing, Keychain locked out) is dropped
  /// for this run and shared_preferences used instead.
  static Future<Prefs> load({TokenVault? vault}) async {
    final sp = await SharedPreferences.getInstance();
    var instance = sp.getInt(_kInstance);
    if (instance == null) {
      instance = randomClientInstance();
      await sp.setInt(_kInstance, instance);
    }
    vault ??= TokenVault.platform();
    final legacy = sp.getString(_kToken);
    var token = legacy;
    if (vault != null) {
      try {
        token = await vault.read();
        if (token == null && legacy != null) {
          await vault.write(legacy);
          token = legacy;
        }
        if (legacy != null) await sp.remove(_kToken);
      } catch (e) {
        debugPrint('femme prefs: token vault unavailable: $e');
        vault = null;
        token = legacy;
      }
    }
    return current = Prefs._(sp, instance, vault, token);
  }

  String? get host => _sp.getString(_kHost);
  String? get token => _token;

  /// Keeps [token] (null removes it) in the vault, or in shared_preferences without one.
  Future<void> _saveToken(String? token) async {
    _token = token;
    final v = _vault;
    if (v != null) {
      try {
        await v.write(token);
        await _sp.remove(_kToken);
        return;
      } catch (e) {
        debugPrint('femme prefs: token vault write failed: $e');
        _vault = null;
      }
    }
    await (token == null ? _sp.remove(_kToken) : _sp.setString(_kToken, token));
  }

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
    await _saveToken(token);
    notifyListeners();
  }

  Future<void> forget() async {
    await _saveToken(null);
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
