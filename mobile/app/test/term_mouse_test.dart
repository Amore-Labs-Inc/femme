import 'dart:async';

import 'package:femme_mobile/net/models.dart' hide Row;
import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/state/term.dart';
import 'package:femme_mobile/ui/terminal/keys.dart';
import 'package:femme_mobile/ui/terminal/mouse.dart';
import 'package:femme_mobile/ui/terminal/painter.dart';
import 'package:femme_mobile/ui/terminal/surface.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter/gestures.dart';
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

  List<Map> get mouse => [
    for (final m in sent.whereType<Map>())
      if (m.containsKey('Mouse')) m['Mouse'] as Map,
  ];
}

Map ev(int seq, String kind, int col, int row, {String mods = ''}) => {
  'seq': seq,
  'ev': {'kind': kind, 'button': 0, 'col': col, 'row': row, 'mods': mods},
};

/// A 30x10 screen with [modes] on a surface at the top left of a 400x300 box.
Future<(TermView, _Port)> pumpSurface(
  WidgetTester tester,
  Set<String> modes, {
  StickyMods? mods,
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
        screen: Screen.alt,
        cols: 30,
        rows: 10,
        topAbs: 0,
        historyLoAbs: 0,
        cursor: const Cursor(row: 0, col: 0),
        modes: modes,
        newStyles: const [],
        rowPatches: const [
          RowPatch(abs: 0, runs: [(0, 'htop')]),
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
            builder: (_, _) =>
                TermSurface(view: view, onTap: () {}, mods: mods),
          ),
        ),
      ),
    ),
  );
  await tester.pump(const Duration(milliseconds: 10));
  addTearDown(view.dispose);
  return (view, port);
}

void main() {
  test('Mouse encodes as serde does for TermUp::Mouse', () {
    expect(
      TermUp.mouse(7, MouseKind.scrollDown, col: 3, row: 4, mods: {Mods.ctrl}),
      {
        'Mouse': {
          'seq': 7,
          'ev': {
            'kind': 'ScrollDown',
            'button': 0,
            'col': 3,
            'row': 4,
            'mods': 'CTRL',
          },
        },
      },
    );
  });

  test('pixel to cell: floor, panned, clamped to the screen', () {
    const w = 8.0, h = 16.0;
    (int, int) at(double x, double y, {double dx = 0}) =>
        touchCell(Offset(x, y), cellW: w, cellH: h, cols: 30, rows: 10, dx: dx);
    expect(at(0, 0), (0, 0));
    expect(at(17, 33), (2, 2));
    expect(at(17, 33, dx: 16), (4, 2));
    expect(at(-5, -5), (0, 0));
    expect(at(9999, 9999), (29, 9));
  });

  test('which modes report the mouse', () {
    expect(reportsMouse(const {}), isFalse);
    expect(reportsMouse({Modes.sgrMouse}), isFalse);
    expect(reportsMouse({Modes.mouseClick}), isTrue);
    expect(reportsDrag({Modes.mouseClick}), isFalse);
    expect(reportsDrag({Modes.mouseDrag}), isTrue);
    expect(reportsDrag({Modes.mouseMotion}), isTrue);
  });

  testWidgets('a tap is a left press + release at its cell', (tester) async {
    final (_, port) = await pumpSurface(tester, {
      Modes.mouseClick,
      Modes.sgrMouse,
    });
    final m = CellMetrics.measure(13);
    await tester.tapAt(Offset(m.width * 5.5, m.height * 2.5));
    expect(port.mouse, [ev(1, 'Press', 5, 2), ev(2, 'Release', 5, 2)]);
  });

  testWidgets('a vertical pan turns the wheel, one notch per cell', (
    tester,
  ) async {
    final (_, port) = await pumpSurface(tester, {Modes.mouseClick});
    final m = CellMetrics.measure(13);
    final g = await tester.startGesture(Offset(m.width * 3.5, m.height * 4.5));
    await g.moveBy(const Offset(0, 40)); // past the slop
    await tester.pump();
    final before = port.mouse.length;
    for (var i = 0; i < 3; i++) {
      await g.moveBy(Offset(0, m.height));
      await tester.pump();
    }
    final notches = port.mouse.skip(before).toList();
    expect(notches, hasLength(3));
    expect(notches.map((e) => (e['ev'] as Map)['kind']).toSet(), {'ScrollUp'});
    expect((notches.first['ev'] as Map)['col'], 3);
    await g.moveBy(
      Offset(0, -2 * m.height),
    ); // the rest left over takes part of one
    await tester.pump();
    expect((port.mouse.last['ev'] as Map)['kind'], 'ScrollDown');
    await g.up();
    expect(
      port.sent.whereType<Map>().any((s) => s.containsKey('GetRows')),
      isFalse,
    );
  });

  testWidgets('drag mode: long press holds the button, moving drags it', (
    tester,
  ) async {
    final (_, port) = await pumpSurface(tester, {Modes.mouseDrag});
    final m = CellMetrics.measure(13);
    final g = await tester.startGesture(Offset(m.width * 1.5, m.height * 1.5));
    await tester.pump(kLongPressTimeout + const Duration(milliseconds: 50));
    await g.moveBy(Offset(m.width * 2, 0));
    await tester.pump();
    await g.up();
    await tester.pump();
    expect(port.mouse, [
      ev(1, 'Press', 1, 1),
      ev(2, 'Move', 3, 1),
      ev(3, 'Release', 3, 1),
    ]);
  });

  testWidgets('no mouse mode: no mouse events, a pan scrolls', (tester) async {
    final (_, port) = await pumpSurface(tester, {Modes.alternateScroll});
    await tester.tapAt(const Offset(20, 20));
    final g = await tester.startGesture(const Offset(20, 20));
    await tester.pump(kLongPressTimeout + const Duration(milliseconds: 50));
    await g.up();
    await tester.drag(find.byType(TermSurface), const Offset(0, 200));
    await tester.pump();
    expect(port.mouse, isEmpty);
    // The alternate screen with ALTERNATE_SCROLL turns the pan into arrow keys instead.
    expect(port.sent.whereType<Map>().any((s) => s.containsKey('Key')), isTrue);
  });

  testWidgets('a tap carries the sticky Shift, which is then used up', (
    tester,
  ) async {
    final mods = StickyMods()..tapShift();
    final (_, port) = await pumpSurface(tester, {
      Modes.mouseClick,
      Modes.sgrMouse,
    }, mods: mods);
    final m = CellMetrics.measure(13);
    await tester.tapAt(Offset(m.width * 5.5, m.height * 2.5));
    expect(port.mouse, [
      ev(1, 'Press', 5, 2, mods: 'SHIFT'),
      ev(2, 'Release', 5, 2, mods: 'SHIFT'),
    ]);
    expect(mods.shift, isFalse);
    await tester.tapAt(Offset(m.width * 5.5, m.height * 2.5));
    expect((port.mouse.last['ev'] as Map)['mods'], '');
  });

  testWidgets('mods are spelled as bitflags; a locked one stays', (
    tester,
  ) async {
    final mods = StickyMods()
      ..tapCtrl()
      ..tapCtrl()
      ..tapShift();
    final (_, port) = await pumpSurface(tester, {Modes.mouseClick}, mods: mods);
    await tester.tapAt(const Offset(20, 20));
    await tester.tapAt(const Offset(20, 20));
    expect(
      [for (final e in port.mouse) (e['ev'] as Map)['mods']],
      ['SHIFT | CTRL', 'SHIFT | CTRL', 'CTRL', 'CTRL'],
    );
    expect(mods.ctrlLocked, isTrue);
  });

  testWidgets('wheel notches carry the sticky Alt, used up on lift', (
    tester,
  ) async {
    final mods = StickyMods()..tapAlt();
    final (_, port) = await pumpSurface(tester, {Modes.mouseClick}, mods: mods);
    final m = CellMetrics.measure(13);
    final g = await tester.startGesture(const Offset(100, 100));
    await g.moveBy(const Offset(0, 40));
    await tester.pump();
    await g.moveBy(Offset(0, m.height * 2));
    await tester.pump();
    expect(port.mouse, isNotEmpty);
    expect(port.mouse.map((e) => (e['ev'] as Map)['mods']).toSet(), {'ALT'});
    expect(mods.alt, isTrue, reason: 'held for the whole gesture');
    await g.up();
    await tester.pump();
    expect(mods.alt, isFalse);
  });
}
