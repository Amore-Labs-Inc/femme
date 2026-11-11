import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:femme_mobile/ui/link_status.dart';
import 'package:femme_mobile/ui/me/pairing.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// A link that never dials and counts reconnects.
class QuietLink extends Link {
  QuietLink() : super(clientInstance: 1);
  int reconnects = 0;

  @override
  void connect(String host, String token) {}
  @override
  void pause() {}
  @override
  void reconnect() => reconnects++;
}

Future<QuietLink> app(WidgetTester t) async {
  SharedPreferences.setMockInitialValues({'host': 'ws://box', 'token': 'tok'});
  ChatController.resetCaches();
  PackageInfo.setMockInitialValues(
    appName: 'FEMME',
    packageName: 'x',
    version: '1.0.0',
    buildNumber: '1',
    buildSignature: '',
  );
  final prefs = await Prefs.load();
  final link = QuietLink();
  await t.pumpWidget(
    FemmeApp(prefs: prefs, link: link, links: const Stream.empty()),
  );
  await t.pump();
  return link;
}

Future<void> settle(WidgetTester t) async {
  for (var i = 0; i < 6; i++) {
    await t.pump(const Duration(milliseconds: 200));
  }
}

final failed = (
  what: 'SessionLabel',
  why: 'NotFound: no such session',
  at: DateTime(2026, 11, 11, 12),
);

void main() {
  test('linkLine says not enrolled; linkDetail has the whole story', () {
    const refused = LinkState(
      LinkPhase.offline,
      reason: 'not enrolled',
      notEnrolled: true,
    );
    expect(linkLine(refused), 'not enrolled: pair again');
    final s = LinkState(
      LinkPhase.offline,
      reason: 'connection refused',
      lastFailed: failed,
    );
    expect(
      linkDetail(s, host: 'wss://box', now: DateTime(2026, 11, 11, 12, 5)),
      'offline: connection refused\n'
      'box: wss://box\n'
      'last failed: SessionLabel: NotFound: no such session (5m ago)',
    );
  });

  testWidgets('tapping the link line shows the full status and reconnects', (
    t,
  ) async {
    final link = await app(t);
    link.state.value = LinkState(
      LinkPhase.offline,
      reason: 'connection refused',
      lastFailed: failed,
    );
    await t.pump();
    await t.tap(find.byKey(const ValueKey('link-line')));
    await settle(t);
    expect(find.byType(LinkStatusSheet), findsOneWidget);
    expect(
      find.textContaining('last failed: SessionLabel: NotFound'),
      findsOneWidget,
    );
    await t.tap(find.text('Reconnect'));
    await settle(t);
    expect(link.reconnects, 1);
    expect(find.byType(LinkStatusSheet), findsNothing);
  });

  testWidgets('a refused device: the line says so and opens pairing', (
    t,
  ) async {
    final link = await app(t);
    link.state.value = const LinkState(
      LinkPhase.offline,
      reason: 'not enrolled',
      notEnrolled: true,
    );
    await t.pump();
    expect(find.text('not enrolled: pair again'), findsOneWidget);
    await t.tap(find.byKey(const ValueKey('link-line')));
    await settle(t);
    expect(find.text('Pair again'), findsOneWidget);
    expect(find.byType(PairForm), findsOneWidget);
  });
}
