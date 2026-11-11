import 'package:femme_mobile/state/prefs.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

class MemVault implements TokenVault {
  MemVault([this.value]);
  String? value;
  bool broken = false;

  @override
  Future<String?> read() async {
    if (broken) throw MissingPluginException('no vault');
    return value;
  }

  @override
  Future<void> write(String? token) async {
    if (broken) throw MissingPluginException('no vault');
    value = token;
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test(
    'a token in shared_preferences moves to the vault on first load',
    () async {
      SharedPreferences.setMockInitialValues({'host': 'h', 'token': 'old'});
      final vault = MemVault();
      final prefs = await Prefs.load(vault: vault);
      expect(prefs.token, 'old');
      expect(prefs.paired, isTrue);
      expect(vault.value, 'old');
      final sp = await SharedPreferences.getInstance();
      expect(sp.getString('token'), isNull);
    },
  );

  test(
    'pairing and forgetting write the vault, not shared_preferences',
    () async {
      SharedPreferences.setMockInitialValues({});
      final vault = MemVault('kept');
      final prefs = await Prefs.load(vault: vault);
      expect(prefs.token, 'kept');
      await prefs.setPairing('h', 'new');
      expect(vault.value, 'new');
      final sp = await SharedPreferences.getInstance();
      expect(sp.getString('token'), isNull);
      await prefs.forget();
      expect(vault.value, isNull);
      expect(prefs.paired, isFalse);
    },
  );

  test('a broken vault falls back to shared_preferences', () async {
    SharedPreferences.setMockInitialValues({'host': 'h', 'token': 'old'});
    final prefs = await Prefs.load(vault: MemVault()..broken = true);
    expect(prefs.token, 'old');
    await prefs.setPairing('h', 'new');
    final sp = await SharedPreferences.getInstance();
    expect(sp.getString('token'), 'new');
  });

  test(
    'without a vault (Linux, tests) the token stays in shared_preferences',
    () async {
      SharedPreferences.setMockInitialValues({'host': 'h', 'token': 'old'});
      final prefs = await Prefs.load();
      expect(prefs.token, 'old');
      final sp = await SharedPreferences.getInstance();
      expect(sp.getString('token'), 'old');
    },
  );
}
