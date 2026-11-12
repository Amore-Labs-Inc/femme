import 'dart:convert';

import '../net/models.dart';
import '../net/rpc.dart';

enum OutboxState {
  /// Sent (or waiting for the link); no answer yet.
  sending,

  /// The box answered `Ok`: its `Pending` row is on the way.
  accepted,

  /// The request failed or timed out; [OutboxEntry.error] says why. `Store.retry` resends it.
  failed,
}

/// A message the user sent that the box's `UserMessage` row has not replaced yet. Shown as an
/// optimistic bubble at the end of the transcript.
class OutboxEntry {
  OutboxEntry({
    required this.op,
    required this.sid,
    required this.text,
    required this.how,
    required this.attachments,
    required this.ts,
  });
  final String op;
  final String sid;
  final String text;
  final PromptHow how;
  final List<Attachment> attachments;
  final int ts;
  OutboxState state = OutboxState.sending;
  String? error;

  /// The failure was the link's (timeout, lost connection), not the box's refusal: the store
  /// sends it again on the next connection.
  bool retryable = false;

  /// Restored from disk after a restart and not sent on this run yet: the next connection
  /// sends it (the box dedups on [op], so a prompt that did arrive before is not doubled).
  bool unsent = false;

  /// What is kept across app restarts: everything needed to send it again. Attachments are
  /// already blobs on the box (`BlobPut` ran before the send), so name + hash is enough.
  Map<String, Object?> toJson() => {
    'op': op,
    'sid': sid,
    'text': text,
    'how': how.name,
    'attachments': [
      for (final a in attachments)
        {'name': a.name, 'blob': base64Encode(a.blob)},
    ],
    'ts': ts,
    'state': state.name,
    'error': error,
    'retryable': retryable,
  };

  /// The inverse of [toJson]; null for anything malformed (a newer app's format, a bad write).
  /// A restored entry that was still on its way comes back as [unsent].
  static OutboxEntry? fromJson(Object? j) {
    try {
      final m = j as Map<String, Object?>;
      final o = OutboxEntry(
        op: m['op'] as String,
        sid: m['sid'] as String,
        text: m['text'] as String,
        how: PromptHow.values.byName(m['how'] as String),
        attachments: [
          for (final a in m['attachments'] as List)
            Attachment(
              (a as Map)['name'] as String,
              base64Decode(a['blob'] as String),
            ),
        ],
        ts: m['ts'] as int,
      );
      if (m['state'] == OutboxState.failed.name) {
        o.state = OutboxState.failed;
        o.error = m['error'] as String?;
        o.retryable = m['retryable'] == true;
      } else {
        o.unsent = true;
      }
      return o;
    } catch (_) {
      return null;
    }
  }

  Object get request =>
      Req.prompt(sid, text, attachments: attachments, how: how);

  /// The entry as a transcript row: a `Pending` user message (status `Failed` once it failed),
  /// item id `local:<op>`, after every real item.
  Item asItem(int seq) => Item(
    sid: sid,
    itemId: 'local:$op',
    seq: seq,
    rev: 0,
    kind: ItemKind.userMessage,
    status: state == OutboxState.failed
        ? ItemStatus.failed
        : ItemStatus.pending,
    preview: text,
    body: jsonEncode({'text': text, 'op': op, 'local': true}),
    ts: ts,
  );
}
