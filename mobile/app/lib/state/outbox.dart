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
