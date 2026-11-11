import 'package:femme_mobile/state/box.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  final now = DateTime.utc(2026, 11, 11, 12);
  String at(Duration ago) => now.subtract(ago).toIso8601String();

  test('ago is rounded like the board', () {
    expect(agoText(const Duration(minutes: 20)), '20m ago');
    expect(agoText(const Duration(minutes: 89)), '1h ago');
    expect(agoText(const Duration(minutes: 90)), '2h ago');
    expect(agoText(const Duration(days: 3)), '3d ago');
  });

  test('the usage note: fresh, old, last good, none', () {
    final a = parseAccounts('''
{"cswap":{"accounts":[
 {"number":1,"usage":{"fiveHour":{"pct":1}},"usageFetchedAt":"${at(const Duration(minutes: 5))}"},
 {"number":2,"usage":{"fiveHour":{"pct":1}},"usageFetchedAt":"${at(const Duration(minutes: 20))}"},
 {"number":3,"usage":null,"lastGoodUsage":{"fiveHour":{"pct":1}},
  "lastGoodFetchedAt":"${at(const Duration(hours: 3))}","usageError":"http-429"},
 {"number":4,"usageStatus":"token_expired"}
]}}''').accounts!;
    expect(a[0].note(now), isNull);
    expect(a[1].note(now), 'usage read 20m ago');
    expect(a[2].note(now), 'last read 3h ago · usage unavailable (http-429)');
    expect(a[3].note(now), 'token expired');
  });
}
