/// A tool call's one-line summary (BODIES.md §4.1): name, hint, first line of the result, and
/// the running / background state.
library;

import 'dart:convert';

import '../../net/models.dart' hide Row;
import '../../net/wire.dart';
import '../../state/chat.dart';

/// Input keys whose first string value is the hint, in this order.
const hintKeys = [
  'command',
  'file_path',
  'path',
  'pattern',
  'url',
  'query',
  'description',
];

class ToolLine {
  const ToolLine({
    required this.name,
    required this.hint,
    this.result,
    this.failed = false,
    this.running = false,
    this.backgroundable = false,
    this.inBackground = false,
    this.linkUrl,
  });
  final String name;
  final String hint;

  /// First non-empty line of the output; null until the result exists.
  final String? result;
  final bool failed;
  final bool running;
  final bool backgroundable;
  final bool inBackground;

  /// An `Artifact` call's page; the hint then reads `· <title> ↗`.
  final String? linkUrl;
}

String firstLine(String s) {
  for (final l in s.split('\n')) {
    final t = l.trim();
    if (t.isNotEmpty) return t;
  }
  return '';
}

/// The hint of a call's input: first line of the first string among [hintKeys].
String hintOf(Object? input) {
  if (input is! Map) return '';
  for (final k in hintKeys) {
    final v = input[k];
    if (v is String) return firstLine(v);
  }
  return '';
}

/// [call] and [result] bodies are passed decoded (null when not at hand: in progress, blob not
/// fetched, cut). [endedTasks] = call ids an `Event` with `task.call` named.
ToolLine toolLine({
  Item? call,
  Json? callBody,
  Item? result,
  Json? resultBody,
  Set<String> endedTasks = const {},
}) {
  var name = '';
  var hint = '';
  if (callBody != null && callBody['name'] is String) {
    name = callBody['name'] as String;
    hint = hintOf(callBody['input']);
  } else if (call != null) {
    final raw = call.body;
    final salvaged = raw == null ? null : stringField(raw, 'name');
    final p = call.preview.trim();
    final sp = p.indexOf(RegExp(r'\s'));
    name = salvaged ?? (sp < 0 ? p : p.substring(0, sp));
    hint = sp < 0 ? '' : firstLine(p.substring(sp + 1));
  } else {
    name = 'result';
  }

  String? res;
  var isError = false;
  Json? link;
  if (result != null) {
    final out = resultBody?['output'];
    res = firstLine(out is String ? out : result.preview);
    isError = resultBody?['is_error'] == true;
    final l = resultBody?['link'];
    if (l is Json) link = l;
  }
  final failed =
      isError ||
      call?.status == ItemStatus.failed ||
      result?.status == ItemStatus.failed;
  final running = result == null && call?.status == ItemStatus.inProgress;
  final task = callBody?['task'];
  final id = call?.itemId ?? '';

  String? url;
  if (link != null && !failed && link['url'] is String) {
    url = link['url'] as String;
    name = 'artifact';
    hint = '· ${link['title'] ?? url} ↗';
    final v = link['version'];
    res = link['updated'] == true && v != null ? 'updated · v$v' : null;
  }

  return ToolLine(
    name: name,
    hint: hint,
    result: res,
    failed: failed,
    running: running,
    backgroundable: running && task == 'foreground',
    inBackground: task == 'background' && !endedTasks.contains(id),
    linkUrl: url,
  );
}

/// What an opened row shows as input: the bare command for exactly `{command}`, else pretty
/// JSON; nothing for `{}` or null.
String inputText(Object? input) {
  if (input == null) return '';
  if (input is Map) {
    if (input.isEmpty) return '';
    if (input.length == 1 && input['command'] is String) {
      return input['command'] as String;
    }
  }
  return const JsonEncoder.withIndent('  ').convert(input);
}

/// Caps a block at [maxLines] lines and [maxBytes] characters; says so at the end.
String capped(String s, {int maxLines = 400, int maxBytes = 24000}) {
  var t = s;
  var cut = false;
  if (t.length > maxBytes) {
    t = t.substring(0, maxBytes);
    cut = true;
  }
  final lines = t.split('\n');
  if (lines.length > maxLines) {
    t = lines.take(maxLines).join('\n');
    cut = true;
  }
  return cut ? '$t\n… (cut)' : t;
}

/// A run of calls as a sentence: `Ran 3 commands, read 2 files`.
String runSentence(List<String> names) {
  final counts = <String, int>{};
  for (final n in names) {
    final k = switch (n) {
      'Bash' || 'shell' || 'exec_command' || 'commandExecution' => 'ran',
      'Read' => 'read',
      'Grep' || 'Glob' || 'WebSearch' || 'webSearch' => 'searched',
      'Edit' ||
      'Write' ||
      'MultiEdit' ||
      'NotebookEdit' ||
      'apply_patch' ||
      'fileChange' => 'edited',
      'WebFetch' => 'fetched',
      _ => '=$n',
    };
    counts[k] = (counts[k] ?? 0) + 1;
  }
  String plural(int n, String one, String many) => n == 1 ? one : many;
  final parts = [
    for (final e in counts.entries)
      switch (e.key) {
        'ran' => 'ran ${e.value} ${plural(e.value, 'command', 'commands')}',
        'read' => 'read ${e.value} ${plural(e.value, 'file', 'files')}',
        'searched' => 'searched ${e.value} ${plural(e.value, 'time', 'times')}',
        'edited' => 'edited ${e.value} ${plural(e.value, 'file', 'files')}',
        'fetched' => 'fetched ${e.value} ${plural(e.value, 'page', 'pages')}',
        _ =>
          '${e.value} ${e.key.substring(1)} ${plural(e.value, 'call', 'calls')}',
      },
  ];
  final s = parts.join(', ');
  return s.isEmpty ? s : s[0].toUpperCase() + s.substring(1);
}
