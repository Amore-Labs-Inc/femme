import 'dart:async';

import 'package:femme_mobile/net/models.dart' hide Row;
import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/state/term.dart';
import 'package:femme_mobile/ui/terminal/painter.dart';
import 'package:femme_mobile/ui/terminal/surface.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

class _Port implements TermPort {
  final ctl = StreamController<TermDown>();
  final sent = <Object>[];
  @override
  Stream<TermDown> get down => ctl.stream;
  @override
  Future<String?> get closed => Completer<String?>().future;
  @override
  void send(Object up) => sent.add(up);
  @override
  void close() {}

  List<String> get mouseKinds => [
    for (final m in sent.whereType<Map>())
      if (m['Mouse'] case {'ev': {'kind': final String k}}) k,
  ];
}

/// A 30x10 screen on a 400x300 surface: the primary screen with scrollback from
/// [historyLoAbs] up to row 500, or the alternate one with [modes].
Future<(TermView, _Port)> pumpSurface(
  WidgetTester tester, {
  Screen screen = Screen.primary,
  Set<String> modes = const {},
  int historyLoAbs = 0,
}) async {
  final port = _Port();
  final view = TermView(
    pane: const PaneId(sid, 1),
    opener: (_, _, _) => port,
    clientInstance: 1,
    isOnline: () => true,
    resizeDebounce: Duration.zero,
  )..start();
  port.ctl.add(
    FrameDown(
      Frame(
        epoch: 1,
        rev: 1,
        baseRev: 0,
        appliedSeq: 0,
        echoSeq: 0,
        screen: screen,
        cols: 30,
        rows: 10,
        topAbs: screen == Screen.primary ? 500 : 0,
        historyLoAbs: screen == Screen.primary ? historyLoAbs : 0,
        cursor: const Cursor(row: 0, col: 0),
        modes: modes,
        newStyles: const [],
        rowPatches: [
          RowPatch(abs: screen == Screen.primary ? 500 : 0, runs: [(0, r'$')]),
        ],
      ),
    ),
  );
  await tester.pump();
  await tester.pumpWidget(
    CupertinoApp(
      home: Align(
        alignment: Alignment.topLeft,
        child: SizedBox(
          width: 400,
          height: 300,
          child: ListenableBuilder(
            listenable: view,
            builder: (_, _) => TermSurface(view: view, onTap: () {}),
          ),
        ),
      ),
    ),
  );
  await tester.pump(const Duration(milliseconds: 10));
  addTearDown(view.dispose);
  return (view, port);
}

/// The absolute row drawn at the top of the surface.
int topRow(WidgetTester tester) =>
    (tester
                .widget<CustomPaint>(
                  find
                      .descendant(
                        of: find.byType(TermSurface),
                        matching: find.byType(CustomPaint),
                      )
                      .first,
                )
                .painter!
            as TermPainter)
        .topAbs;

/// A quick flick down (toward older rows) from the middle of the surface.
Future<void> flick(WidgetTester tester, {double speed = 2500}) =>
    tester.flingFrom(const Offset(200, 100), const Offset(0, 120), speed);

/// [n] samples of [read], 100 ms apart.
Future<List<int>> sample(
  WidgetTester tester,
  int n,
  int Function() read,
) async {
  final out = <int>[];
  for (var i = 0; i < n; i++) {
    await tester.pump(const Duration(milliseconds: 100));
    out.add(read());
  }
  return out;
}

void main() {
  testWidgets('a fling keeps scrolling the scrollback after lift and decays', (
    tester,
  ) async {
    await pumpSurface(tester);
    expect(topRow(tester), 500);
    await flick(tester);
    await tester.pump();
    final atLift = topRow(tester);
    expect(atLift, lessThan(500), reason: 'the pan itself scrolled');
    final tops = await sample(tester, 8, () => topRow(tester));
    expect(tops.first, lessThan(atLift), reason: 'still moving after lift');
    final steps = [
      atLift - tops.first,
      for (var i = 1; i < tops.length; i++) tops[i - 1] - tops[i],
    ];
    expect(steps.every((s) => s >= 0), isTrue, reason: '$steps');
    expect(steps.last, lessThan(steps.first), reason: 'slows down: $steps');
    // It comes to rest by itself.
    await tester.pump(const Duration(seconds: 5));
    final rest = topRow(tester);
    await tester.pump(const Duration(milliseconds: 500));
    expect(topRow(tester), rest);
  });

  testWidgets('a touch stops a fling, and that tap does nothing else', (
    tester,
  ) async {
    final (_, port) = await pumpSurface(
      tester,
      screen: Screen.alt,
      modes: {Modes.mouseClick},
    );
    await flick(tester);
    await tester.pump(const Duration(milliseconds: 50));
    final g = await tester.startGesture(const Offset(100, 100));
    await tester.pump();
    final n = port.mouseKinds.length;
    await tester.pump(const Duration(milliseconds: 500));
    expect(port.mouseKinds.length, n, reason: 'no notches after the touch');
    await g.up();
    await tester.pump();
    expect(port.mouseKinds.skip(n), isEmpty, reason: 'no click from it');
    expect(tester.binding.transientCallbackCount, 0);
  });

  testWidgets('a touch stops a scrollback fling where it is', (tester) async {
    await pumpSurface(tester);
    await flick(tester);
    await tester.pump(const Duration(milliseconds: 50));
    final g = await tester.startGesture(const Offset(100, 100));
    await tester.pump();
    final at = topRow(tester);
    await tester.pump(const Duration(milliseconds: 500));
    expect(topRow(tester), at);
    await g.up();
  });

  testWidgets('a fling stops at the oldest row', (tester) async {
    await pumpSurface(tester, historyLoAbs: 490);
    await flick(tester, speed: 4000);
    await sample(tester, 3, () => topRow(tester));
    expect(topRow(tester), 490);
  });

  testWidgets('mouse mode: wheel notches keep coming while the fling decays', (
    tester,
  ) async {
    final (_, port) = await pumpSurface(
      tester,
      screen: Screen.alt,
      modes: {Modes.mouseClick},
    );
    await flick(tester);
    await tester.pump();
    final atLift = port.mouseKinds.length;
    expect(atLift, greaterThan(0));
    final counts = await sample(tester, 8, () => port.mouseKinds.length);
    expect(counts.first, greaterThan(atLift), reason: 'notches after lift');
    final steps = [
      counts.first - atLift,
      for (var i = 1; i < counts.length; i++) counts[i] - counts[i - 1],
    ];
    expect(steps.last, lessThan(steps.first), reason: 'slows down: $steps');
    expect(port.mouseKinds.toSet(), {'ScrollUp'});
    expect(
      port.sent.whereType<Map>().any((s) => s.containsKey('GetRows')),
      isFalse,
    );
  });
}
