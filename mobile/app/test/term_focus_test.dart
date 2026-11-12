import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/state/term.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';

import 'term_view_test.dart' show FakePort, frame, pane;

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
      retryDelay: Duration.zero,
    )..start();
  });

  tearDown(() => view.dispose());

  test('Focus encodes as serde writes a newtype variant', () {
    expect(TermUp.focus(true), {'Focus': true});
    expect(TermUp.focus(false), {'Focus': false});
  });

  test(
    'focus is said on a change only, and not before the stream is live',
    () async {
      view.setFocus(false); // starts unfocused: nothing to say
      view.setFocus(true);
      expect(ports.last.of('Focus'), isEmpty);
      await ports.last.push(FrameDown(frame(rev: 1)));
      expect(ports.last.of('Focus'), [true], reason: 'said once live');
      view.setFocus(true);
      view.setFocus(false);
      view.setFocus(true);
      expect(ports.last.of('Focus'), [true, false, true]);
    },
  );

  test('a new stream is told the focus again', () async {
    await ports.last.push(FrameDown(frame(rev: 1)));
    expect(ports.last.of('Focus'), isEmpty, reason: 'never told: not resent');
    view.setFocus(true);
    view.setFocus(false);
    ports.last.end('reset');
    await Future<void>.delayed(Duration.zero);
    await Future<void>.delayed(Duration.zero);
    expect(ports, hasLength(2));
    await ports.last.push(FrameDown(frame(rev: 2)));
    expect(ports.last.of('Focus'), [false]);
  });
}
