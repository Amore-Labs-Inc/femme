import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/box.dart';
import 'package:flutter_test/flutter_test.dart';

Pr pr({bool? mergeable = true, String? fixing, bool merging = false}) =>
    Pr.fromJson({
      'number': 7,
      'title': 't',
      'branch': 'b',
      'author': 'a',
      'url': 'u',
      'body': '',
      'additions': 1,
      'deletions': 2,
      'video': null,
      'mergeable': mergeable,
      'fixing': fixing,
      'merging': merging,
    });

void main() {
  test('PR row badge', () {
    expect(prBadge(pr()), isNull);
    expect(prBadge(pr(mergeable: false)), PrBadge.conflicts);
    expect(prBadge(pr(mergeable: null)), PrBadge.checking);
    expect(prBadge(pr(mergeable: false, fixing: 'S')), PrBadge.fixing);
    expect(
      prBadge(pr(mergeable: false, fixing: 'S', merging: true)),
      PrBadge.merging,
    );
    expect(prBadgeLabel(PrBadge.merging), 'merging…');
    expect(prsUnsettled([pr()]), isFalse);
    expect(prsUnsettled([pr(), pr(mergeable: null)]), isTrue);
  });

  test('Unsupported reads as a too-old box', () {
    expect(
      describeError(const FemmeError('Unsupported', 'x')),
      'box too old for this',
    );
    expect(isUnsupported(const FemmeError('Unsupported', '')), isTrue);
    expect(describeError(const FemmeError('NotFound', 'gone')), 'gone');
  });

  test('accounts: fresh, stale, sign-in, cswap error', () {
    const body = '''
{"cswap":{"accounts":[
 {"number":1,"email":"ada@x","active":true,"usage":{
   "fiveHour":{"pct":31.0,"resetsAt":"2026-11-03T13:00:00+00:00"},
   "sevenDay":{"pct":48.4,"countdown":"4d 20h"},
   "scoped":[{"name":"Fable","pct":71.0}]},
  "loginExpiresAt":"2026-11-04T10:00:00Z"},
 {"number":4,"alias":"m","usageStatus":"unavailable","usage":null,
  "lastGoodUsage":{"fiveHour":{"pct":5.0}},"usageError":"http-429"},
 {"bad":true}
]}}''';
    final r = parseAccounts(body);
    expect(r.error, isNull);
    final a = r.accounts!;
    expect(a.length, 2);
    expect(a[0].title, 'ada@x');
    expect(a[0].active, isTrue);
    final now = DateTime.utc(2026, 11, 3, 10);
    expect(a[0].fiveHour!.text(now), '31% · 3h 0m');
    expect(a[0].sevenDay!.text(now), '48% · 4d 20h');
    expect(a[0].scoped.single.name, 'Fable');
    expect(a[0].signIn(now), ('sign-in ends in 24 h', Level.bad));
    expect(a[1].title, 'm');
    expect(a[1].stale, isTrue);
    expect(a[1].reason, 'usage unavailable (http-429)');
    expect(parseAccounts('{"cswap":{"error":"boom"}}').error, 'boom');
    expect(parseAccounts('{"cswap":null}').accounts, isNull);
  });

  test('keys and meta', () {
    final k = parseKeys(
      '{"keys":[{"name":"ci","hint":"a1b2","created":1793000000,"account":"the box"},{"x":1}]}',
    );
    expect(k.single.name, 'ci');
    expect(k.single.hint, 'a1b2');
    expect(parseKeys('[{"name":"a"}]').single.account, 'the box');
    expect(parseNewKeySecret('{"name":"a","key":"sk-1"}'), 'sk-1');
    expect(parseMetaCap('{"cap":60}'), 60);
    expect(validKeyName('ci-bot'), isTrue);
    expect(validKeyName('CI bot'), isFalse);
  });
}
