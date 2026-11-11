import 'package:femme_mobile/ui/me.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';

void main() {
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
