import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/ui/chat/media.dart';
import 'package:femme_mobile/ui/chat/rows.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

/// Answers `BlobGet` with [text], or fails while [fail] is set.
class BlobBox implements Requester {
  bool fail = true;
  String text = '{"text":"the long answer"}';
  int asked = 0;

  @override
  bool isOnline = true;

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) async {
    asked++;
    if (fail) throw const FemmeError('NotFound', 'no such blob');
    return Resp('Blob', utf8.encode(text));
  }
}

Item blobItem() => Item(
  sid: sid,
  itemId: 'msg_long',
  seq: 1,
  rev: 1,
  kind: ItemKind.assistantMessage,
  status: ItemStatus.completed,
  preview: 'the long…',
  blobHash: List.filled(32, 7),
  ts: 1794300000000,
);

void main() {
  setUp(ChatController.resetCaches);

  test(
    'a failed blob fetch is shown, not asked again at once, and retried',
    () async {
      final box = BlobBox();
      final c = ChatController(sid, box);
      final item = blobItem();
      expect(c.bodyText(item), isNull);
      await pumpEventQueue();
      expect(box.asked, 1);
      expect(c.blobError(item), 'no such blob');
      // drawing again does not hammer the box
      expect(c.bodyText(item), isNull);
      await pumpEventQueue();
      expect(box.asked, 1);
      // retry: the error goes and the next draw asks again
      box.fail = false;
      c.retryBlob(item);
      expect(c.blobError(item), isNull);
      c.bodyText(item);
      await pumpEventQueue();
      expect(box.asked, 2);
      expect(c.json(item)?['text'], 'the long answer');
      expect(c.blobError(item), isNull);
    },
  );

  test('offline is a failure too, not a busy loop', () async {
    final box = BlobBox()..isOnline = false;
    final c = ChatController(sid, box);
    var notes = 0;
    c.addListener(() => notes++);
    c.bodyText(blobItem());
    await pumpEventQueue();
    expect(box.asked, 0);
    expect(c.blobError(blobItem()), contains('offline'));
    c.bodyText(blobItem());
    await pumpEventQueue();
    expect(notes, 1);
  });

  testWidgets('the row says couldn\'t load · retry and a tap asks again', (
    tester,
  ) async {
    final box = BlobBox();
    final c = ChatController(sid, box);
    final item = blobItem();
    c.bodyText(item);
    await tester.pump();
    await tester.pumpWidget(
      CupertinoApp(
        theme: femmeTheme(Brightness.light),
        home: ChatScope(
          controller: c,
          child: BlobRetry(item, child: const Text('row')),
        ),
      ),
    );
    expect(find.text("couldn't load · retry"), findsOneWidget);
    box.fail = false;
    await tester.tap(find.text("couldn't load · retry"));
    await tester.pump();
    expect(find.text("couldn't load · retry"), findsNothing);
  });
}
