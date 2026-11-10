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

  /// Streaming text per in-progress item id, and its length in characters (runes).
  final _deltas = <String, StringBuffer>{};
  final _deltaLen = <String, int>{};

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
    _deltaLen.remove(item.itemId);
    _sorted = null;
    return true;
  }

  bool remove(String itemId) {
    _deltas.remove(itemId);
    _deltaLen.remove(itemId);
    _sorted = null;
    return byId.remove(itemId) != null;
  }

  void clear() {
    byId.clear();
    _deltas.clear();
    _deltaLen.clear();
    _sorted = null;
    rev = null;
    nextBefore = null;
    olderUnknown = true;
  }

  /// Applies a `Delta`: [offset] is the number of characters already applied. A resent delta
  /// (offset inside the buffer) cuts the buffer back first; one past its end (a gap) is dropped
  /// and the item's next row fills it in. Returns whether the text changed.
  bool applyDelta(String itemId, int offset, String text) {
    var buf = _deltas[itemId];
    var len = _deltaLen[itemId] ?? 0;
    if (buf == null) {
      // The body's text, not the preview: offsets count from the start of the full text.
      final seed = byId[itemId]?.bodyJson?['text'] as String? ?? '';
      buf = StringBuffer(seed);
      len = seed.runes.length;
    }
    if (offset > len) return false;
    if (offset < len) {
      final kept = String.fromCharCodes(buf.toString().runes.take(offset));
      buf = StringBuffer(kept);
      len = offset;
    }
    buf.write(text);
    _deltas[itemId] = buf;
    _deltaLen[itemId] = len + text.runes.length;
    return true;
  }

  /// The text to show for [item]: its streaming buffer while one is open, else its body's text.
  String textOf(Item item) => _deltas[item.itemId]?.toString() ?? item.text;

  bool streaming(String itemId) => _deltas.containsKey(itemId);
}
