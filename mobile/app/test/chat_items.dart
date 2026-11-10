import 'dart:convert';

import 'package:femme_mobile/net/models.dart';

import 'fixtures.dart';

var _seq = 0;

/// An item for the chat tests: [body] is encoded unless it is already a string.
Item it(
  String id,
  ItemKind kind, {
  Object? body,
  ItemStatus status = ItemStatus.completed,
  String preview = '',
  int? ts,
  String? parent,
}) {
  _seq++;
  return Item(
    sid: sid,
    itemId: id,
    seq: _seq,
    rev: _seq,
    kind: kind,
    status: status,
    preview: preview,
    body: body == null ? null : (body is String ? body : jsonEncode(body)),
    ts: ts ?? 1794300000000 + _seq * 1000,
    parent: parent,
  );
}

Item call(
  String id,
  String name,
  Map<String, Object?> input, {
  ItemStatus status = ItemStatus.completed,
  String? task,
  String? parent,
}) => it(
  id,
  ItemKind.toolCall,
  status: status,
  parent: parent,
  body: {'call_id': id, 'input': input, 'name': name, 'task': ?task},
);

Item result(
  String callId,
  String output, {
  bool error = false,
  Map<String, Object?>? link,
}) => it(
  '$callId:result',
  ItemKind.toolResult,
  status: error ? ItemStatus.failed : ItemStatus.completed,
  body: {'call_id': callId, 'is_error': error, 'link': ?link, 'output': output},
);
