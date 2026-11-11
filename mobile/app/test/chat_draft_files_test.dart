import 'dart:typed_data';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat/attachments.dart';
import 'package:flutter_test/flutter_test.dart';

/// Answers `BlobPut` with a fresh hash each time and `Prompt` as [prompt] says.
class BoxStub implements Requester {
  final sent = <Map>[];
  int puts = 0;

  /// Prompts refused with "its upload is gone" before one is taken.
  int goneFor = 0;

  @override
  bool isOnline = true;

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) async {
    final m = req as Map;
    sent.add(m);
    if (m['BlobPut'] != null) {
      puts++;
      return Resp('BlobHash', List.filled(32, puts));
    }
    if (m['Prompt'] != null) {
      if (goneFor > 0) {
        goneFor--;
        throw const FemmeError(
          'Bad',
          'attachment a.png: its upload is gone, send it again',
        );
      }
      return const Resp('Ok', null);
    }
    throw StateError('unexpected $m');
  }

  List<List<int>> get promptBlobs => [
    for (final m in sent)
      if (m['Prompt'] != null)
        for (final a in (m['Prompt'] as Map)['attachments'] as List)
          (a as Map)['blob'] as List<int>,
  ];
}

DraftFile ready(String name, int hashByte) =>
    DraftFile(name, Uint8List.fromList([1, 2, 3]))
      ..state = UploadState.ready
      ..hash = List.filled(32, hashByte);

void main() {
  test(
    'an upload the box swept goes up again and the prompt is resent',
    () async {
      final box = BoxStub()..goneFor = 1;
      final store = Store(keeper: null)..attachRequester(box);
      final f = ready('a.png', 0);
      await sendWithFiles(store, box, 's1', 'look', sent: [f]);
      expect(box.puts, 1);
      expect(box.promptBlobs, [List.filled(32, 0), List.filled(32, 1)]);
      expect(f.hash, List.filled(32, 1));
      // the failed bubble is gone, only the resent one stays
      expect(store.outbox.length, 1);
      expect(store.outbox.single.attachments.single.blob, List.filled(32, 1));
    },
  );

  test('gone twice: only one re-upload, then the failure shows', () async {
    final box = BoxStub()..goneFor = 2;
    final store = Store(keeper: null)..attachRequester(box);
    await expectLater(
      sendWithFiles(store, box, 's1', 'look', sent: [ready('a.png', 0)]),
      throwsA(isA<FemmeError>()),
    );
    expect(box.puts, 1);
  });

  test('other errors and file-less prompts are not retried', () async {
    final box = BoxStub()..goneFor = 1;
    final store = Store(keeper: null)..attachRequester(box);
    await expectLater(
      sendWithFiles(store, box, 's1', 'look'),
      throwsA(isA<FemmeError>()),
    );
    expect(box.puts, 0);
    expect(isUploadGone(const FemmeError('Bad', 'no such session')), isFalse);
  });
}
