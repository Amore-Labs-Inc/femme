import 'dart:async';
import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/box_watch.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:shared_preferences/shared_preferences.dart';

final now = DateTime.utc(2026, 11, 10, 12);

String accountsBody(List<Duration?> loginLeft) => jsonEncode({
  'cswap': {
    'accounts': [
      for (final (i, left) in loginLeft.indexed)
        {
          'number': i + 1,
          'alias': 'acct$i',
          'loginExpiresAt': left == null
              ? null
              : now.add(left).toIso8601String(),
        },
    ],
  },
});

Resp boardOk(String body) => Resp('BoardApi', {'status': 200, 'body': body});

void main() {
  group('signInAttention', () {
    test('red when a sign-in ended or ends within two days', () {
      expect(
        signInAttention(accountsBody([null, Duration(days: 5)]), now),
        isFalse,
      );
      expect(signInAttention(accountsBody([Duration(days: 30)]), now), isFalse);
      expect(
        signInAttention(accountsBody([null, Duration(hours: 30)]), now),
        isTrue,
      );
      expect(
        signInAttention(
          accountsBody([Duration(days: 20), -Duration(hours: 1)]),
          now,
        ),
        isTrue,
      );
    });

    test('no accounts, an error or junk is not red', () {
      expect(signInAttention('{"cswap": {"error": "locked"}}', now), isFalse);
      expect(signInAttention('{"aws": {}}', now), isFalse);
      expect(signInAttention('nope', now), isFalse);
    });
  });

  group('BoxWatch', () {
    testWidgets(
      'reads when the link comes up, again on the timer, never offline',
      (t) async {
        final link = ValueNotifier(const LinkState(LinkPhase.offline));
        final calls = <Object>[];
        var body = accountsBody([Duration(days: 30)]);
        final w = BoxWatch(
          (req) async {
            calls.add(req);
            return boardOk(body);
          },
          link,
          now: () => now,
        );
        await t.pump();
        expect(calls, isEmpty);

        link.value = const LinkState(LinkPhase.online);
        await t.pump();
        expect(calls.single, Req.boardApi('GET', '/api/accounts'));
        expect(w.attention.value, isFalse);

        body = accountsBody([Duration(hours: 5)]);
        await t.pump(const Duration(minutes: 10));
        expect(calls.length, 2);
        expect(w.attention.value, isTrue);

        link.value = const LinkState(LinkPhase.offline);
        await t.pump(const Duration(minutes: 30));
        expect(calls.length, 2);
        expect(w.attention.value, isTrue, reason: 'kept while offline');
        w.dispose();
      },
    );

    testWidgets('a failed read keeps the last answer', (t) async {
      final link = ValueNotifier(const LinkState(LinkPhase.online));
      var fail = false;
      final w = BoxWatch(
        (req) async {
          if (fail) throw const LinkLost('gone');
          return boardOk(accountsBody([-Duration(days: 1)]));
        },
        link,
        now: () => now,
      );
      await t.pump();
      expect(w.attention.value, isTrue);
      fail = true;
      await t.pump(const Duration(minutes: 10));
      expect(w.attention.value, isTrue);
      w.dispose();
    });
  });

  testWidgets('the Box tab gets a red dot while attention is set', (t) async {
    SharedPreferences.setMockInitialValues({});
    PackageInfo.setMockInitialValues(
      appName: 'FEMME',
      packageName: 'x',
      version: '1.0.0',
      buildNumber: '1',
      buildSignature: '',
    );
    final prefs = await Prefs.load();
    final link = Link(clientInstance: 1);
    final watch = BoxWatch(
      (_) => Completer<Resp>().future,
      ValueNotifier(const LinkState(LinkPhase.offline)),
    );
    await t.pumpWidget(
      Femme(
        store: Store(),
        link: link,
        prefs: prefs,
        boxWatch: watch,
        child: const CupertinoApp(home: Shell()),
      ),
    );
    final dot = find.byKey(const ValueKey('box-attention'));
    expect(dot, findsNothing);
    watch.attention.value = true;
    await t.pump();
    expect(dot, findsOneWidget);
    expect(t.getSize(dot), const Size(8, 8));
    watch.attention.value = false;
    await t.pump();
    expect(dot, findsNothing);
    await t.pumpWidget(const SizedBox());
  });
}
