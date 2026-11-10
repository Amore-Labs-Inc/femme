import 'dart:async';

import 'package:femme_mobile/net/models.dart' hide Row;
import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/state/term.dart';
import 'package:femme_mobile/ui/terminal/extra_keys.dart';
import 'package:femme_mobile/ui/terminal/keys.dart';
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
}

void main() {
  testWidgets('draws a styled screen, scrolls into scrollback, keys tap', (
    tester,
  ) async {
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
          screen: Screen.primary,
          cols: 30,
          rows: 3,
          topAbs: 500,
          historyLoAbs: 0,
          cursor: const Cursor(row: 1, col: 4),
          modes: const {},
          newStyles: [
            (
              1,
              Style.fromJson({
                'fg': {'Indexed': 1},
                'bg': {
                  'Rgb': [0, 0, 80],
                },
                'underline': 'Default',
                'attrs': 'BOLD | UNDERLINE | INVERSE',
              }),
            ),
          ],
          rowPatches: const [
            RowPatch(
              abs: 500,
              runs: [(1, 'error'), (0, ' 漢字 ok')],
              wideCols: [7, 9],
            ),
            RowPatch(abs: 501, runs: [(0, r'$ ls')], wrapped: true),
            RowPatch(abs: 502, runs: [(0, 'é 🙂')], wideCols: [3]),
          ],
        ),
      ),
    );
    await tester.pump();
    final mods = StickyMods();
    final keys = <ExtraKey>[];
    await tester.pumpWidget(
      CupertinoApp(
        home: CupertinoPageScaffold(
          child: Column(
            children: [
              Expanded(
                child: ListenableBuilder(
                  listenable: view,
                  builder: (_, _) => TermSurface(view: view, onTap: () {}),
                ),
              ),
              ExtraKeysRow(mods: mods, onKey: keys.add),
            ],
          ),
        ),
      ),
    );
    await tester.pump(const Duration(milliseconds: 10));
    expect(tester.takeException(), isNull);
    expect(
      port.sent.whereType<Map>().any((m) => m.containsKey('Resize')),
      isTrue,
    );

    await tester.drag(find.byType(TermSurface), const Offset(0, 300));
    await tester.pump();
    expect(
      port.sent.whereType<Map>().any((m) => m.containsKey('GetRows')),
      isTrue,
    );
    expect(find.text('↓ live'), findsOneWidget);

    await tester.tap(find.text('Esc'));
    expect(keys, [ExtraKey.esc]);
    view.dispose();
  });
}
