import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/box/accounts.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

class _BoxLink extends Link {
  _BoxLink() : super(clientInstance: 1);
  final setCaps = <Object>[];

  @override
  bool get isOnline => true;

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
    bool track = true,
  }) async {
    if (req == Req.caps) return const Resp('Caps', []);
    if (req is Map && req.containsKey('SetCap')) {
      setCaps.add(req);
      return const Resp('Ok', null);
    }
    if (req is Map && req.containsKey('BoardApi')) {
      final path = (req['BoardApi'] as Map)['path'];
      final body = path == '/api/meta'
          ? '{"cap":80}'
          : '{"cswap":{"accounts":[{"number":1,"alias":"one","active":true}]}}';
      return Resp('BoardApi', {'status': 200, 'body': body});
    }
    throw StateError('unexpected $req');
  }
}

void main() {
  testWidgets('a cap typed just before the section closes is still sent', (
    t,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final link = _BoxLink();
    await t.pumpWidget(
      Femme(
        store: Store(),
        link: link,
        prefs: prefs,
        child: const CupertinoApp(
          home: CupertinoPageScaffold(
            child: SingleChildScrollView(child: AccountsSection()),
          ),
        ),
      ),
    );
    await t.pump();
    await t.pump();
    expect(find.text('one'), findsOneWidget);

    await t.tap(find.byIcon(CupertinoIcons.plus_circle).first);
    await t.pump(const Duration(milliseconds: 100));
    expect(link.setCaps, isEmpty, reason: 'debounced');

    await t.pumpWidget(const SizedBox());
    expect(t.takeException(), isNull);
    expect(link.setCaps, [Req.setCap(1, fiveHour: 80)]);
  });
}
