import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/ui/chat/composer.dart';
import 'package:flutter_test/flutter_test.dart';

import 'chat_items.dart';
import 'fixtures.dart';

void main() {
  test('stop takes the prompt back until a reply row shows', () {
    final prompt = it('u1', ItemKind.userMessage, status: ItemStatus.pending);
    final read = it('e1', ItemKind.event);
    expect(takesBack([]), isFalse);
    expect(takesBack([prompt, read]), isTrue);
    expect(
      takesBack([prompt, read, it('a1', ItemKind.assistantMessage)]),
      isFalse,
    );
    expect(takesBack([prompt, call('t1', 'Bash', {})]), isFalse);
    // a subagent's rows are not the main thread's reply
    expect(
      takesBack([prompt, it('s1', ItemKind.assistantMessage, parent: 't0')]),
      isTrue,
    );
    expect(
      takesBack([it('u2', ItemKind.userMessage, status: ItemStatus.failed)]),
      isFalse,
    );
  });

  test('TakeBack and its answer on the wire', () {
    expect(Req.takeBack(sid), {
      'TakeBack': {'sid': sid},
    });
    final back = Resp.fromJson({
      'TakenBack': {'text': 'fix it'},
    });
    expect(back.takenBack, 'fix it');
    expect(Resp.fromJson('Ok').takenBack, isNull);
  });
}
