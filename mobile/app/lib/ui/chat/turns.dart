/// Lays a transcript's items out as rows: a tool call and its result as one row, read marks,
/// compaction folds, notable turn ends, and the folding of a finished turn's work into one
/// `worked for 3m · 12 tools` line (BODIES.md §3.11, §4.1). Pure: tests drive it directly.
library;

import '../../net/models.dart' hide Row;
import '../../net/wire.dart';
import '../../state/chat.dart';
import 'attached.dart';
import 'tool_summary.dart';

sealed class Entry {
  const Entry();

  /// Stable per row: the item id, or the first inner row's for a fold.
  String get key;
}

/// A user, peer, assistant, plan, question, event or thinking row.
class ItemEntry extends Entry {
  const ItemEntry(this.item, {this.ms});
  final Item item;

  /// For thinking: how long it took (up to the next item), when known.
  final int? ms;
  @override
  String get key => item.itemId;
}

/// A tool call and its result as one row. [call] is null for a result whose call is not held.
class ToolEntry extends Entry {
  const ToolEntry(this.call, this.result);
  final Item? call;
  final Item? result;
  @override
  String get key => call?.itemId ?? result!.itemId;

  bool get running => result == null && call?.status == ItemStatus.inProgress;
  bool get failed =>
      call?.status == ItemStatus.failed || result?.status == ItemStatus.failed;
}

/// Where the agent read a queued message: a thin line naming it.
class ReadEntry extends Entry {
  const ReadEntry(this.event, this.target);
  final Item event;
  final String target;
  @override
  String get key => event.itemId;
}

/// Claude's post-compaction summary, folded.
class CompactionEntry extends Entry {
  const CompactionEntry(this.item);
  final Item item;
  @override
  String get key => item.itemId;
}

/// A turn end worth a line: interrupted, output limit, failed (red), other.
class TurnEndEntry extends Entry {
  const TurnEndEntry(this.item, this.text, {this.red = false});
  final Item item;
  final String text;
  final bool red;
  @override
  String get key => item.itemId;
}

/// Several work rows behind one line; tap opens them.
class FoldEntry extends Entry {
  const FoldEntry(this.inner, this.label, {this.failed = 0});
  final List<Entry> inner;
  final String label;
  final int failed;
  @override
  String get key => 'fold:${inner.first.key}';
}

/// The id of the call a result answers: its body's `call_id`, else the id minus `:result`.
String? callIdOf(Item result, Json? body) {
  final c = body?['call_id'];
  if (c is String) return c;
  const s = ':result';
  final id = result.itemId;
  return id.endsWith(s) ? id.substring(0, id.length - s.length) : null;
}

/// The text of a notable turn end, or null for a plain `EndTurn`.
({String text, bool red})? turnEndNote(Json? body) {
  final stop = body?['stop'];
  if (stop == null || stop == 'EndTurn') return null;
  if (stop == 'Interrupted') return (text: '· interrupted', red: false);
  if (stop == 'MaxTokens') {
    return (text: '· stopped: output limit reached', red: false);
  }
  if (stop is Map && stop['Error'] != null) {
    return (text: '· turn failed: ${stop['Error']}', red: true);
  }
  if (stop is Map && stop['Other'] != null) {
    return (text: '· turn ended: ${stop['Other']}', red: false);
  }
  return null;
}

/// The rows for [items] (one view: the main thread or one subagent), in order. [body] decodes
/// an item's body when at hand.
List<Entry> layout(
  List<Item> items, {
  required Json? Function(Item) body,
  Set<String> endedTasks = const {},
}) {
  final calls = <String>{
    for (final i in items)
      if (i.kind == ItemKind.toolCall) i.itemId,
  };
  final results = <String, Item>{};
  for (final i in items) {
    if (i.kind != ItemKind.toolResult) continue;
    final c = callIdOf(i, body(i));
    if (c != null) results[c] = i;
  }

  final out = <Entry>[];
  var segment = <Entry>[];
  int? segmentStart;

  void close(Item? end) {
    if (end == null) {
      out.addAll(_foldRuns(segment, body, endedTasks));
    } else {
      out.addAll(
        _foldTurn(segment, end, body(end), segmentStart, endedTasks, body),
      );
    }
    segment = <Entry>[];
    segmentStart = null;
  }

  for (var n = 0; n < items.length; n++) {
    final i = items[n];
    segmentStart ??= i.ts;
    switch (i.kind) {
      case ItemKind.turnStart:
        if (segment.isNotEmpty) close(null);
      case ItemKind.turnEnd:
        close(i);
        final note = turnEndNote(body(i));
        if (note != null) out.add(TurnEndEntry(i, note.text, red: note.red));
      case ItemKind.toolCall:
        segment.add(ToolEntry(i, results[i.itemId]));
      case ItemKind.toolResult:
        final c = callIdOf(i, body(i));
        if (c == null || !calls.contains(c)) segment.add(ToolEntry(null, i));
      case ItemKind.event:
        final b = body(i);
        final read = b?['read'];
        if (read is String) {
          final prev = segment.isNotEmpty
              ? segment.last
              : (out.isNotEmpty ? out.last : null);
          final above = prev is ItemEntry && prev.item.itemId == read;
          if (!above) segment.add(ReadEntry(i, read));
          continue;
        }
        if (n + 1 < items.length &&
            items[n + 1].kind == ItemKind.userMessage &&
            items[n + 1].text.startsWith(compactionPrefix)) {
          continue;
        }
        segment.add(ItemEntry(i));
      case ItemKind.userMessage:
        if (i.text.startsWith(compactionPrefix)) {
          segment.add(CompactionEntry(i));
        } else {
          segment.add(ItemEntry(i));
        }
      case ItemKind.thinking:
        final next = n + 1 < items.length ? items[n + 1].ts : null;
        final ms = next != null && i.status != ItemStatus.inProgress
            ? next - i.ts
            : null;
        segment.add(ItemEntry(i, ms: ms));
      default:
        segment.add(ItemEntry(i));
    }
  }
  close(null);
  return out;
}

bool _isWork(Entry e) =>
    e is ToolEntry || (e is ItemEntry && e.item.kind == ItemKind.thinking);

bool _keepsOut(Entry e, Json? Function(Item) body, Set<String> ended) {
  if (e is! ToolEntry) return false;
  if (e.running) return true;
  final c = e.call;
  if (c == null) return false;
  final b = body(c);
  return b?['task'] == 'background' && !ended.contains(c.itemId);
}

/// A finished turn: its thinking and tool rows behind one line at the first one's place.
List<Entry> _foldTurn(
  List<Entry> seg,
  Item end,
  Json? endBody,
  int? start,
  Set<String> ended,
  Json? Function(Item) body,
) {
  final work = [
    for (final e in seg)
      if (_isWork(e) && !_keepsOut(e, body, ended)) e,
  ];
  if (work.isEmpty) return seg;
  final tools = work.whereType<ToolEntry>().length;
  final failed = work.whereType<ToolEntry>().where((t) => t.failed).length;
  final ms =
      (endBody?['duration_ms'] as num?)?.toInt() ??
      (start == null ? null : end.ts - start);
  final time = ms == null ? '' : ' for ${duration(ms)}';
  final label = tools == 0
      ? 'thought$time'
      : 'worked$time · $tools ${tools == 1 ? 'tool' : 'tools'}'
            '${failed > 0 ? ' · $failed failed' : ''}';
  final fold = FoldEntry(work, label, failed: failed);
  final out = <Entry>[];
  var placed = false;
  for (final e in seg) {
    if (work.contains(e)) {
      if (!placed) out.add(fold);
      placed = true;
    } else {
      out.add(e);
    }
  }
  return out;
}

/// The turn still running: each run of consecutive finished tool calls folds all but its last
/// call into a sentence (`Ran 3 commands, read 2 files`) when that hides two or more.
List<Entry> _foldRuns(
  List<Entry> seg,
  Json? Function(Item) body,
  Set<String> ended,
) {
  final out = <Entry>[];
  var run = <ToolEntry>[];
  void flush() {
    if (run.length >= 3) {
      final folded = run.sublist(0, run.length - 1);
      final names = [
        for (final t in folded)
          toolLine(
            call: t.call,
            callBody: t.call == null ? null : body(t.call!),
          ).name,
      ];
      final failed = folded.where((t) => t.failed).length;
      out.add(FoldEntry(folded, runSentence(names), failed: failed));
      out.add(run.last);
    } else {
      out.addAll(run);
    }
    run = <ToolEntry>[];
  }

  for (final e in seg) {
    if (e is ToolEntry && !_keepsOut(e, body, ended)) {
      run.add(e);
    } else {
      flush();
      out.add(e);
    }
  }
  flush();
  return out;
}

/// Call ids whose background task ended (an `Event` with `task.call`).
Set<String> endedTasksOf(List<Item> items, Json? Function(Item) body) => {
  for (final i in items)
    if (i.kind == ItemKind.event)
      if (body(i)?['task'] case {'call': final String c}) c,
};

/// Whether a task the turn waits for (a running foreground Bash or Agent call) could move to
/// the background: the gate of `Background all` (desktop live.rs `background(None)`).
bool hasForegroundTask(List<Item> items, Json? Function(Item) body) {
  final answered = <String>{
    for (final i in items)
      if (i.kind == ItemKind.toolResult) ?callIdOf(i, body(i)),
  };
  return items.any(
    (i) =>
        i.kind == ItemKind.toolCall &&
        !answered.contains(i.itemId) &&
        toolLine(call: i, callBody: body(i)).backgroundable,
  );
}

/// The working line's verb from the last item: `Thinking`, `Writing`, `Running a tool`.
String workingVerb(List<Item> items) {
  for (final i in items.reversed) {
    switch (i.kind) {
      case ItemKind.thinking:
        return 'Thinking';
      case ItemKind.assistantMessage:
        return 'Writing';
      case ItemKind.toolCall || ItemKind.toolResult:
        return 'Running a tool';
      case ItemKind.userMessage:
        return 'Thinking';
      default:
        continue;
    }
  }
  return 'Working';
}

/// Unix ms of the last user message (the working line counts from it).
int? lastUserTs(List<Item> items) {
  for (final i in items.reversed) {
    if (i.kind == ItemKind.userMessage) return i.ts;
  }
  return null;
}

/// One subagent on show in the strip above the transcript (desktop agents.rs `Chip`).
class AgentChip {
  AgentChip({
    required this.id,
    required this.label,
    required this.running,
    required this.since,
  });

  /// The `Agent` call's id: the `parent` of the subagent's items.
  final String id;

  /// What it was asked to do (the call's `description`), else its kind.
  final String label;

  /// Its call has no result yet, or it runs in the background and has not been said to stop.
  bool running;

  /// Unix ms of the call.
  final int since;
  bool _background = false;
  bool _earlier = false;
}

/// The subagents on show, oldest first: the `Agent` / `Task` calls of the current turn, and the
/// background ones of earlier turns until an `Event` says they stopped. [items] are all the
/// session's items in order (a subagent's own items mark a background one as at work).
List<AgentChip> agentChips(List<Item> items, Json? Function(Item) body) {
  final out = <AgentChip>[];
  AgentChip? find(String id) => out.where((c) => c.id == id).firstOrNull;
  for (final i in items) {
    final p = i.parent;
    if (p != null) {
      final c = find(p);
      if (c != null && c._background) c.running = true;
      continue;
    }
    switch (i.kind) {
      case ItemKind.turnEnd:
        out.removeWhere((c) => !c.running);
        for (final c in out) {
          c._earlier = true;
        }
      case ItemKind.toolCall:
        final b = body(i);
        final line = toolLine(call: i, callBody: b);
        if (line.name != 'Agent' && line.name != 'Task') continue;
        final input = b?['input'];
        String label;
        if (input is Map) {
          final d = input['description'];
          final t = input['subagent_type'];
          label = d is String && d.trim().isNotEmpty
              ? d
              : (t is String ? t : 'agent');
        } else {
          // a long prompt makes the body a blob: the preview is `Agent <description>`
          label = line.hint.isEmpty ? 'agent' : line.hint;
        }
        out.add(
          AgentChip(
            id: i.itemId,
            label: label,
            running: i.status != ItemStatus.failed,
            since: i.ts,
          ),
        );
      case ItemKind.toolResult:
        final b = body(i);
        final id = callIdOf(i, b);
        final c = id == null ? null : find(id);
        if (c == null) continue;
        final out0 = b?['output'];
        final text = out0 is String ? out0 : i.preview;
        c._background =
            i.status != ItemStatus.failed &&
            b?['is_error'] != true &&
            text.startsWith('Async agent launched');
        c.running = c._background;
      case ItemKind.event:
        if (body(i)?['task'] case {'call': final String id}) {
          final c = find(id);
          if (c == null) continue;
          if (c._earlier) {
            out.remove(c);
          } else {
            c.running = false;
          }
        }
      default:
        break;
    }
  }
  return out;
}

/// `4s`, `1m 12s`: how long a subagent has run (desktop agents.rs `elapsed`).
String agentElapsed(int since, int now) {
  final s = ((now - since) ~/ 1000).clamp(0, 1 << 31);
  if (s < 60) return '${s}s';
  return '${s ~/ 60}m ${(s % 60).toString().padLeft(2, '0')}s';
}
