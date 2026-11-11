import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/terminal.dart';
import 'package:femme_mobile/ui/terminal/screen.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

Session session(
  String id, {
  String agent = 'Shell',
  String mode = 'Headless',
  String status = 'Idle',
  int created = 1794300000000,
  int? closed,
}) => Session.fromJson({
  ...sessionJson(id: id, status: status),
  'agent': agent,
  'mode': mode,
  'created': created,
  'closed': closed,
});

Pane pane(String sid, int no, {String kind = 'Shell', String title = ''}) =>
    Pane(id: PaneId(sid, no), rev: 1, kind: wireKind(kind), title: title);

PaneKind wireKind(String k) => k == 'Agent' ? PaneKind.agent : PaneKind.shell;

void put(Store store, Session s, List<Pane> panes) {
  store.sessions[s.id] = s;
  for (final p in panes) {
    store.panes[p.id.key] = p;
  }
}

Future<void> show(WidgetTester tester, Store store, String sid) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await Prefs.load();
  await tester.pumpWidget(
    Femme(
      store: store,
      link: Link(clientInstance: 1),
      prefs: prefs,
      child: CupertinoApp(
        theme: femmeTheme(Brightness.light),
        home: TerminalScreen(sid: sid),
      ),
    ),
  );
  await tester.pump();
}

void main() {
  test('ended: a shell\'s first pane or a TUI\'s agent pane closes the '
      'session, another pane only itself', () {
    final store = Store();
    put(store, session(sid), [pane(sid, 1), pane(sid, 2)]);
    put(store, session(sid2, agent: 'Claude', mode: 'Tui'), [
      pane(sid2, 0, kind: 'Agent'),
      pane(sid2, 1),
    ]);
    expect(endedTitle(store, const PaneId(sid, 1)), 'session closed');
    expect(endedTitle(store, const PaneId(sid, 2)), 'pane closed');
    expect(endedTitle(store, const PaneId(sid2, 0)), 'session closed');
    expect(endedTitle(store, const PaneId(sid2, 1)), 'pane closed');

    store.sessions[sid] = session(sid, status: 'Closed', closed: 1);
    expect(sessionEnded(store, sid), isTrue);
    expect(endedTitle(store, const PaneId(sid, 2)), 'session closed');
    store.sessions.remove(sid);
    expect(sessionEnded(store, sid), isTrue);
  });

  testWidgets('a closed shell session shows the session closed card', (
    tester,
  ) async {
    final store = Store();
    put(store, session(sid, status: 'Closed', closed: 1), []);
    await show(tester, store, sid);
    expect(find.text('session closed'), findsOneWidget);
    expect(find.text('pick another one in the list'), findsOneWidget);
    expect(find.text('Open a shell'), findsNothing);
  });

  test('tabs: every live session\'s panes, oldest session first', () {
    const sid3 = '01JD3Q8Z9X4K2M7N5P6R8S0T3X';
    final store = Store();
    put(store, session(sid, created: 300), [pane(sid, 2), pane(sid, 1)]);
    put(store, session(sid2, agent: 'Claude', created: 100), [
      pane(sid2, 1, kind: 'Agent'),
      pane(sid2, 2),
    ]);
    put(store, session(sid3, created: 50, status: 'Closed', closed: 9), [
      pane(sid3, 1),
    ]);
    expect(
      [for (final p in terminalTabs(store)) p.id],
      const [PaneId(sid2, 2), PaneId(sid, 1), PaneId(sid, 2)],
    );
  });

  test('onShow keeps the pick, else the tab in its place, else the last', () {
    final tabs = [pane(sid, 1), pane(sid, 2), pane(sid2, 1)];
    expect(onShow(tabs, const PaneId(sid2, 1), 0), 2);
    expect(onShow(tabs, const PaneId(sid, 9), 1), 1);
    expect(onShow(tabs, const PaneId(sid, 9), 7), 2);
    expect(onShow(const [], const PaneId(sid, 1), 0), isNull);
  });

  testWidgets('the tab strip lists the panes of other sessions too', (
    tester,
  ) async {
    final store = Store();
    put(store, session(sid, created: 100), [pane(sid, 1, title: 'zsh one')]);
    put(store, session(sid2, created: 200), [pane(sid2, 1, title: 'vim two')]);
    await show(tester, store, sid2);
    expect(find.text('zsh one'), findsOneWidget);
    // the open session's pane is on show: its label is the title too
    expect(find.text('vim two'), findsNWidgets(2));
    final strip = tester.widget<CupertinoSlidingSegmentedControl<PaneId>>(
      find.byType(CupertinoSlidingSegmentedControl<PaneId>),
    );
    expect(strip.children.keys.toList(), const [
      PaneId(sid, 1),
      PaneId(sid2, 1),
    ]);
    expect(strip.groupValue, const PaneId(sid2, 1));
  });

  test('closing a running pane asks first; the agent\'s own pane never', () {
    final store = Store();
    put(store, session(sid), [pane(sid, 1), pane(sid, 2)]);
    put(store, session(sid2, agent: 'Claude', mode: 'Tui'), [
      pane(sid2, 0, kind: 'Agent'),
      pane(sid2, 1),
    ]);
    expect(
      closeConfirm(store, const PaneId(sid, 1), 'zsh')?.$3,
      'Close session',
    );
    expect(closeConfirm(store, const PaneId(sid, 2), 'zsh')?.$1, 'Close zsh?');
    expect(closeConfirm(store, const PaneId(sid, 2), 'zsh')?.$3, 'Close pane');
    expect(closeConfirm(store, const PaneId(sid2, 0), 'claude'), isNull);
    expect(closeConfirm(store, const PaneId(sid2, 1), 'sh')?.$3, 'Close pane');
  });

  testWidgets('a running pane has a close button that confirms first', (
    tester,
  ) async {
    final store = Store();
    put(store, session(sid), [
      pane(sid, 1, title: 'zsh'),
      pane(sid, 2, title: 'htop'),
    ]);
    put(store, session(sid2, agent: 'Claude', mode: 'Tui'), [
      pane(sid2, 0, kind: 'Agent', title: 'claude'),
    ]);
    await show(tester, store, sid2);
    // the agent's own pane: no close
    expect(find.byKey(const ValueKey('term-close')), findsNothing);

    await tester.tap(find.text('htop'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('term-close')));
    await tester.pumpAndSettle();
    expect(find.text('Close htop?'), findsOneWidget);
    expect(find.text('The program in it is still running.'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(find.text('Close htop?'), findsNothing);
    expect(store.panes.length, 3);
  });

  testWidgets('the nav bar close and + are named 44 pt buttons', (
    tester,
  ) async {
    final store = Store();
    put(store, session(sid), [pane(sid, 1, title: 'zsh'), pane(sid, 2)]);
    await show(tester, store, sid);
    for (final (key, label) in [
      ('term-close', 'close pane'),
      ('term-new', 'new shell pane'),
    ]) {
      final b = find.byKey(ValueKey(key));
      expect(tester.getSize(b).height, greaterThanOrEqualTo(44));
      expect(tester.getSize(b).width, greaterThanOrEqualTo(44));
      expect(
        find.descendant(of: b, matching: find.bySemanticsLabel(label)),
        findsOneWidget,
      );
    }
  });

  testWidgets('a session without panes offers a filled, rounded, 44 pt '
      'Open a shell', (tester) async {
    final empty = Store();
    put(empty, session(sid2), []);
    await show(tester, empty, sid2);
    final open = find.byKey(const ValueKey('term-open-shell'));
    expect(tester.getSize(open).height, greaterThanOrEqualTo(44));
    final deco = tester
        .widgetList<DecoratedBox>(
          find.descendant(of: open, matching: find.byType(DecoratedBox)),
        )
        .map((d) => d.decoration)
        .whereType<ShapeDecoration>()
        .first;
    expect(
      (deco.shape as RoundedSuperellipseBorder).borderRadius,
      const BorderRadius.all(Radius.circular(12)),
    );
  });

  testWidgets('the closed card is rounded, without border or hard shadow', (
    tester,
  ) async {
    final store = Store();
    put(store, session(sid, status: 'Closed', closed: 1), []);
    await show(tester, store, sid);
    final box = tester
        .widgetList<Container>(
          find.ancestor(
            of: find.text('session closed'),
            matching: find.byType(Container),
          ),
        )
        .map((c) => c.decoration)
        .whereType<BoxDecoration>()
        .first;
    expect(box.border, isNull);
    expect(box.borderRadius, const BorderRadius.all(Radius.circular(12)));
    for (final s in box.boxShadow ?? const <BoxShadow>[]) {
      expect(s.blurRadius, greaterThan(0));
      expect(s.offset.dx, 0);
    }
  });
}
