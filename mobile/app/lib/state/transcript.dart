import 'dart:convert';

import '../net/models.dart';

/// One session's transcript as the store holds it: items by id, the rev cursor, streaming text
/// buffers and the paging state.
class Transcript {
  final byId = <String, Item>{};

  /// Highest rev received through `Changes` for this session (the resume cursor); null until any.
  int? rev;

  /// Subscribers (screens) holding the session open.
  int refs = 0;

  /// The `next_before` of the last page: more history exists before this seq. Null once the
  /// start was reached; [olderUnknown] until the first page was asked.
  int? nextBefore;
  bool olderUnknown = true;
  bool loadingOlder = false;

  /// Streaming text per in-progress item id, as UTF-8 bytes: `Delta.offset` counts bytes of
  /// the daemon's accumulated text (headless.rs: `acc.len()`), and its decoded text.
  final _deltas = <String, List<int>>{};
  final _deltaText = <String, String>{};

  List<Item>? _sorted;

  /// Items ordered by seq (item id breaks ties).
  List<Item> get sorted => _sorted ??= byId.values.toList()
    ..sort((a, b) {
      final c = a.seq.compareTo(b.seq);
      return c != 0 ? c : a.itemId.compareTo(b.itemId);
    });

  int? get oldestSeq => byId.isEmpty ? null : sorted.first.seq;
  int get newestSeq => byId.isEmpty ? 0 : sorted.last.seq;

  /// Stores [item] unless a copy with a higher rev is already held. A newer row replaces the
  /// item's streaming buffer: its text is authoritative from here on.
  bool put(Item item) {
    final old = byId[item.itemId];
    if (old != null && old.rev > item.rev) return false;
    byId[item.itemId] = item;
    _deltas.remove(item.itemId);
    _deltaText.remove(item.itemId);
    _sorted = null;
    return true;
  }

  bool remove(String itemId) {
    _deltas.remove(itemId);
    _deltaText.remove(itemId);
    _sorted = null;
    return byId.remove(itemId) != null;
  }

  void clear() {
    byId.clear();
    _deltas.clear();
    _deltaText.clear();
    _sorted = null;
    rev = null;
    nextBefore = null;
    olderUnknown = true;
  }

  /// Applies a `Delta`: [offset] is the UTF-8 byte offset the daemon appended at. A resent
  /// delta (offset inside the buffer) cuts the buffer back first; one past its end (a gap) is
  /// dropped and the item's next row fills it in; one inside a character is ignored. Returns
  /// whether the text changed.
  bool applyDelta(String itemId, int offset, String text) {
    var buf = _deltas[itemId];
    if (buf == null) {
      // The body's text, not the preview: offsets count from the start of the full text.
      final seed = byId[itemId]?.bodyJson?['text'] as String? ?? '';
      buf = utf8.encode(seed).toList();
    }
    if (offset > buf.length) return false;
    if (offset < buf.length) {
      // a continuation byte at the cut: the offset is inside a character
      if (buf[offset] & 0xC0 == 0x80) return false;
      buf.length = offset;
    }
    buf.addAll(utf8.encode(text));
    _deltas[itemId] = buf;
    _deltaText[itemId] = utf8.decode(buf, allowMalformed: true);
    return true;
  }

  /// The text to show for [item]: its streaming buffer while one is open, else its body's text.
  String textOf(Item item) => _deltaText[item.itemId] ?? item.text;

  bool streaming(String itemId) => _deltas.containsKey(itemId);
}
