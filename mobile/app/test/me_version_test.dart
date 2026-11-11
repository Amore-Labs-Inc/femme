import 'package:femme_mobile/ui/me.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';

void main() {
  test('paired just now, else paired 5m ago', _pairedAgo);
  test('the About version carries the commit when the build has one', () {
    final info = PackageInfo(
      appName: 'FEMME',
      packageName: 'x',
      version: '1.0.0',
      buildNumber: '12',
    );
    expect(appVersionText(null), '—');
    expect(appVersionText(info, sha: ''), '1.0.0 (12)');
    expect(
      appVersionText(info, sha: 'a3ca0990930de7192a2208e649313454a6674c12'),
      '1.0.0 (12) · a3ca099',
    );
    expect(femmeSha, isEmpty, reason: 'no --dart-define in tests');
  });
}

void _pairedAgo() {
  final now = DateTime(2026, 11, 11, 3);
  expect(
    pairedAgo(now.subtract(const Duration(seconds: 20)), now: now),
    'paired just now',
  );
  expect(
    pairedAgo(now.subtract(const Duration(minutes: 5)), now: now),
    'paired 5m ago',
  );
}
