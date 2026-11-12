import 'dart:async';
import 'dart:convert';

import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/state/term.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

class FakePort implements TermPort {
  FakePort(this.epoch, this.haveRev);
  final int epoch;
  final int haveRev;
  final ctl = StreamController<TermDown>();
  final sent = <Object>[];
  final _closed = Completer<String?>();

  @override
  Stream<TermDown> get down => ctl.stream;
  @override
  Future<String?> get closed => _closed.future;
  @override
  void send(Object up) => sent.add(jsonDecode(jsonEncode(up)) as Object);
  @override
  void close() => end(null);

  void end(String? why) {
    if (!_closed.isCompleted) _closed.complete(why);
  }

  /// Delivers [d] and lets the view handle it.
  Future<void> push(TermDown d) async {
    ctl.add(d);
    await Future<void>.delayed(Duration.zero);
  }

  List<String> get tags => [for (final m in sent) (m as Map).keys.first];
  List<Object> of(String tag) => [
    for (final m in sent)
      if ((m as Map).containsKey(tag)) m[tag] as Object,
  ];
}

const pane = PaneId(sid, 2);

Frame frame({
  int epoch = 7,
  required int rev,
  int baseRev = 0,
  int appliedSeq = 0,
  int top = 0,
  int lo = 0,
  int rows = 3,
  int cols = 10,
  List<(int, Style)> styles = const [],
  List<RowPatch> patches = const [],
  String? title,
}) => Frame(
  epoch: epoch,
  rev: rev,
  baseRev: baseRev,
  appliedSeq: appliedSeq,
  echoSeq: appliedSeq,
  screen: Screen.primary,
  cols: cols,
  rows: rows,
  topAbs: top,
  historyLoAbs: lo,
  cursor: const Cursor(),
  modes: const {},
  title: title,
  newStyles: styles,
  rowPatches: patches,
);

RowPatch row(int abs, String text, {int style = 0}) =>
    RowPatch(abs: abs, runs: [(style, text)]);

void main() {
  late List<FakePort> ports;
  late ValueNotifier<bool> online;
  late TermView view;

  setUp(() {
    ports = [];
    online = ValueNotifier(true);
    view = TermView(
      pane: pane,
      opener: (p, epoch, haveRev) {
        final f = FakePort(epoch, haveRev);
        ports.add(f);
        return f;
      },
      clientInstance: 42,
      isOnline: () => online.value,
      online: online,
      resizeDebounce: const Duration(milliseconds: 1),
      retryDelay: Duration.zero,
    )..start();
  });

  tearDown(() => view.dispose());

  test('opens fresh, applies a snapshot then a delta, acks each', () async {
    expect(ports.single.epoch, 0);
    expect(ports.single.haveRev, 0);
    await ports.last.push(
      FrameDown(
        frame(
          rev: 5,
          styles: [
            (1, const Style(attrs: {Attrs.bold})),
          ],
          patches: [row(0, r'$ ls', style: 1), row(1, 'a.txt'), row(2, '')],
          title: 'zsh',
        ),
      ),
    );
    expect(view.live, isTrue);
    expect(view.grid.rev, 5);
    expect(view.grid.row(0)!.text, r'$ ls');
    expect(view.grid.style(1).attrs, {Attrs.bold});
    expect(view.grid.title, 'zsh');
    await ports.last.push(
      FrameDown(frame(rev: 6, baseRev: 5, patches: [row(1, 'b.txt')])),
    );
    expect(view.grid.row(1)!.text, 'b.txt');
    expect(view.grid.row(0)!.text, r'$ ls', reason: 'unpatched rows stay');
    expect(view.grid.title, 'zsh', reason: 'a title is kept until replaced');
    expect(ports.last.of('Ack'), [
      {'rev': 5},
      {'rev': 6},
    ]);
  });

  test('a base_rev it does not hold re-opens once with the rev held', () async {
    await ports.last.push(FrameDown(frame(rev: 5, patches: [row(0, 'x')])));
    await ports.last.push(FrameDown(frame(rev: 10, baseRev: 9)));
    await ports.last.push(FrameDown(frame(rev: 11, baseRev: 10)));
    expect(view.grid.rev, 5);
    final opens = ports.last.of('Open');
    expect(opens, hasLength(1), reason: 'no Open storm while resyncing');
    expect(opens.single, {
      'pane': {'sid': sid, 'no': 2},
      'epoch': 7,
      'have_rev': 5,
      'client_instance': 42,
    });
    // The holder answers with a delta from 5: applied.
    await ports.last.push(FrameDown(frame(rev: 12, baseRev: 5, patches: [])));
    expect(view.grid.rev, 12);
  });

  test('a delta of another epoch resets and asks for a snapshot', () async {
    await ports.last.push(FrameDown(frame(rev: 5, patches: [row(0, 'x')])));
    await ports.last.push(FrameDown(frame(epoch: 8, rev: 3, baseRev: 2)));
    final open = ports.last.of('Open').single as Map;
    expect(open['epoch'], 0);
    expect(open['have_rev'], 0);
    expect(view.grid.row(0), isNull);
  });

  test('the style table grows by frame and a snapshot drops it', () async {
    await ports.last.push(
      FrameDown(
        frame(
          rev: 1,
          styles: [
            (1, const Style(attrs: {'BOLD'})),
          ],
        ),
      ),
    );
    await ports.last.push(
      FrameDown(
        frame(
          rev: 2,
          baseRev: 1,
          styles: [
            (2, const Style(attrs: {'ITALIC'})),
          ],
        ),
      ),
    );
    expect(view.grid.styles.keys, unorderedEquals([1, 2]));
    await ports.last.push(
      FrameDown(
        frame(
          rev: 3,
          styles: [
            (1, const Style(attrs: {'DIM'})),
          ],
        ),
      ),
    );
    expect(view.grid.styles.keys, [1]);
    expect(view.grid.style(1).attrs, {'DIM'});
    expect(view.grid.style(9).attrs, isEmpty, reason: 'unknown = default');
  });

  test('a snapshot keeps scrollback, replaces the visible rows', () async {
    await ports.last.push(
      FrameDown(
        frame(
          rev: 1,
          top: 10,
          patches: [row(8, 'old'), row(10, 'a'), row(11, 'b')],
        ),
      ),
    );
    await ports.last.push(
      FrameDown(frame(rev: 2, top: 10, patches: [row(10, 'c')])),
    );
    expect(view.grid.row(8)!.text, 'old');
    expect(view.grid.row(10)!.text, 'c');
    expect(view.grid.row(11), isNull);
  });

  test(
    'GetRows pages scrollback in chunks of 200, never below history_lo',
    () async {
      await ports.last.push(
        FrameDown(frame(rev: 1, top: 1000, lo: 550, rows: 2)),
      );
      view.needRows(0);
      expect(ports.last.of('GetRows'), [
        {'from_abs': 800, 'to_abs': 1000},
      ]);
      view.needRows(0); // one request at a time
      expect(ports.last.of('GetRows'), hasLength(1));
      await ports.last.push(
        RowsDown([for (var i = 800; i < 1000; i++) row(i, '$i')]),
      );
      expect(view.grid.row(900)!.text, '900');
      expect(ports.last.of('GetRows').last, {'from_abs': 600, 'to_abs': 800});
      await ports.last.push(
        RowsDown([for (var i = 600; i < 800; i++) row(i, '$i')]),
      );
      expect(ports.last.of('GetRows').last, {'from_abs': 550, 'to_abs': 600});
      await ports.last.push(
        RowsDown([for (var i = 550; i < 600; i++) row(i, '$i')]),
      );
      expect(ports.last.of('GetRows'), hasLength(3));
      expect(view.loadingRows, isFalse);
    },
  );

  test('GetRows stops when the holder has nothing more', () async {
    await ports.last.push(FrameDown(frame(rev: 1, top: 300, lo: 0)));
    view.needRows(250);
    expect(ports.last.of('GetRows').single, {'from_abs': 100, 'to_abs': 300});
    await ports.last.push(const RowsDown([]));
    view.needRows(0);
    expect(ports.last.of('GetRows'), hasLength(1));
  });

  test('keys: Ctrl+c is Char(c) with CTRL, seqs increase', () async {
    await ports.last.push(FrameDown(frame(rev: 1, appliedSeq: 0)));
    view.key(KeyCode.char('c'), mods: {Mods.ctrl});
    view.paste('hello');
    expect(ports.last.of('Key').single, {
      'seq': 1,
      'key': {
        'code': {'Char': 'c'},
        'mods': 'CTRL',
      },
    });
    expect(ports.last.of('Paste').single, {'seq': 2, 'text': 'hello'});
  });

  test(
    'input waits for the first frame and continues after applied_seq',
    () async {
      view.key(KeyCode.enter);
      expect(ports.last.of('Key'), isEmpty);
      await ports.last.push(FrameDown(frame(rev: 1, appliedSeq: 40)));
      expect((ports.last.of('Key').single as Map)['seq'], 41);
    },
  );

  test('reconnects with epoch and have_rev, resends unapplied input', () async {
    await ports.last.push(FrameDown(frame(rev: 4, appliedSeq: 0)));
    view.key(KeyCode.char('a'));
    view.key(KeyCode.char('b'));
    online.value = false;
    ports.last.end('connection lost');
    await Future<void>.delayed(Duration.zero);
    expect(ports, hasLength(1), reason: 'waits for the link');
    online.value = true;
    expect(ports, hasLength(2));
    expect(ports.last.epoch, 7);
    expect(ports.last.haveRev, 4);
    // The holder wrote seq 1 before the drop: only `b` (seq 2) goes again.
    await ports.last.push(FrameDown(frame(rev: 5, baseRev: 4, appliedSeq: 1)));
    expect(ports.last.of('Key'), [
      {
        'seq': 2,
        'key': {
          'code': {'Char': 'b'},
          'mods': '',
        },
      },
    ]);
  });

  test('a reset stream while online is retried', () async {
    await ports.last.push(FrameDown(frame(rev: 4)));
    ports.last.end('reset');
    await Future<void>.delayed(const Duration(milliseconds: 5));
    expect(ports, hasLength(2));
  });

  test('exited stops reconnection', () async {
    await ports.last.push(FrameDown(frame(rev: 4)));
    await ports.last.push(const Exited(3));
    ports.last.end(null);
    await Future<void>.delayed(const Duration(milliseconds: 5));
    expect(view.exited, 3);
    expect(ports, hasLength(1));
  });

  test('resize is debounced and skips the size held', () async {
    await ports.last.push(FrameDown(frame(rev: 1, cols: 40, rows: 20)));
    view.setSize(50, 20);
    view.setSize(60, 22);
    await Future<void>.delayed(const Duration(milliseconds: 10));
    expect(ports.last.of('Resize'), [
      {'cols': 60, 'rows': 22},
    ]);
    view.setSize(60, 22);
    await Future<void>.delayed(const Duration(milliseconds: 10));
    expect(ports.last.of('Resize'), hasLength(1));
  });
}
