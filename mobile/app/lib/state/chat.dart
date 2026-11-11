/// View state of one open chat: fetched blobs and pictures, the command list, the answer
/// state of questions, which rows are opened, drafts. Pure helpers (titles, model labels) sit
/// at the top so the header, the composer and the session sheet read the same words.
library;

import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/link.dart';
import '../net/models.dart';
import '../net/rpc.dart';
import '../net/wire.dart';

// ---- words ----

/// The chat's title: name, else label, else the last part of cwd, else the handle.
String sessionTitle(Session s) {
  if (s.name != null && s.name!.isNotEmpty) return s.name!;
  if (s.label != null && s.label!.isNotEmpty) return s.label!;
  final parts = s.cwd.split('/').where((p) => p.isNotEmpty);
  if (parts.isNotEmpty) return parts.last;
  return s.handle;
}

/// `claude-opus-5-5` → `opus 5.5`; `[1m]` and a gateway provider (`openrouter/…`) dropped, a
/// date suffix too. Unknown shapes come back as they are, minus those.
String modelLabel(String? model) {
  if (model == null || model.isEmpty) return '';
  var m = model.replaceAll('[1m]', '').trim();
  if (m.contains('/')) m = m.substring(m.lastIndexOf('/') + 1);
  if (m.startsWith('claude-')) m = m.substring(7);
  m = m.replaceFirst(RegExp(r'-\d{8}$'), '');
  final v = RegExp(r'^([a-z]+)-(\d+)-(\d+)$').firstMatch(m);
  if (v != null) return '${v[1]} ${v[2]}.${v[3]}';
  final w = RegExp(r'^([a-z]+)-(\d+)$').firstMatch(m);
  if (w != null) return '${w[1]} ${w[2]}';
  return m;
}

/// `opus 5.5 · high`, or the agent's name when the model is unknown.
String modelEffort(Session s) {
  final m = modelLabel(s.model);
  final base = m.isEmpty ? s.agent.name : m;
  final e = s.effort;
  return e == null || e.isEmpty ? base : '$base · $e';
}

/// The permission mode's short name (`default` reads `manual`).
String permissionLabel(String? p) => switch (p) {
  null || '' => 'manual',
  'default' => 'manual',
  'acceptEdits' => 'accept edits',
  'bypassPermissions' => 'bypass',
  'dontAsk' => "don't ask",
  _ => p,
};

/// `5s`, `3m 12s`, `1h 4m`.
String duration(int ms) {
  final s = (ms / 1000).round();
  if (s < 60) return '${s}s';
  final m = s ~/ 60;
  if (m < 60) return s % 60 == 0 ? '${m}m' : '${m}m ${s % 60}s';
  final h = m ~/ 60;
  return m % 60 == 0 ? '${h}h' : '${h}h ${m % 60}m';
}

/// `123,456`.
String grouped(int n) {
  final s = n.abs().toString();
  final b = StringBuffer(n < 0 ? '-' : '');
  for (var i = 0; i < s.length; i++) {
    if (i > 0 && (s.length - i) % 3 == 0) b.write(',');
    b.write(s[i]);
  }
  return b.toString();
}

/// `3.4 MB`, `812 KB`, `90 B`.
String byteSize(int n) {
  if (n < 1024) return '$n B';
  if (n < 1024 * 1024) return '${(n / 1024).round()} KB';
  return '${(n / (1024 * 1024)).toStringAsFixed(1)} MB';
}

/// Short reason of a request failure.
String errorText(Object e) => switch (e) {
  FemmeError(:final code, :final msg) => msg.isEmpty ? code : msg,
  LinkLost(:final reason) => 'link lost: $reason',
  TimeoutException() => 'no answer from the box',
  _ => e.toString(),
};

/// Salvages one string field from JSON text that may be cut (`…[truncated]`): bodies have
/// sorted keys, so a field before the cut is whole.
String? stringField(String json, String key) {
  final m = RegExp('"${RegExp.escape(key)}":"((?:[^"\\\\]|\\\\.)*)"')
      .firstMatch(json);
  if (m == null) return null;
  try {
    return jsonDecode('"${m[1]}"') as String;
  } on FormatException {
    return m[1];
  }
}

// ---- the controller ----

/// The answer state of one question this phone answered.
class AnswerState {
  AnswerState({this.sending = false, this.error});
  bool sending;
  String? error;
}

/// One chat's view state. Made by the chat screen, disposed with it; shared caches (pictures,
/// blobs, the command list) are static so a reopened chat does not fetch again.
class ChatController extends ChangeNotifier {
  ChatController(this.sid, this.link);

  final String sid;
  final Requester link;

  // Pictures by `sid\npath`, newest last; capped.
  static final _pics = <String, Uint8List>{};
  static final _picErr = <String, String>{};
  static const _picCap = 60;
  static final _blobs = <String, String>{};
  static final _blobErr = <String, ({String text, DateTime at})>{};

  /// A failed blob fetch is asked again on the first draw after this long (or at once on a
  /// tap of the row's `retry`).
  static const blobRetryAfter = Duration(seconds: 30);
  static List<String>? _commands;
  static DateTime? _commandsAt;

  final _inflight = <String>{};
  final answers = <String, AnswerState>{};

  /// Keys of rows the user opened (tool rows, folds, thinking).
  final opened = <String>{};

  /// Derived from the items on each transcript build (no notification): call ids whose
  /// background task ended, and call ids that have subagent items.
  Set<String> endedTasks = const {};
  Set<String> parents = const {};
  bool _disposed = false;

  @override
  void dispose() {
    _disposed = true;
    super.dispose();
  }

  void _changed() {
    if (!_disposed) notifyListeners();
  }

  void toggle(String key) {
    if (!opened.remove(key)) opened.add(key);
    _changed();
  }

  bool isOpen(String key) => opened.contains(key);

  // ---- bodies ----

  /// The item's body JSON text: inline, or its blob once fetched (fetched on the first ask
  /// when [fetch]). Null while missing.
  String? bodyText(Item item, {bool fetch = true}) {
    if (item.body != null) return item.body;
    final h = item.blobHash;
    if (h == null) return null;
    final key = _hex(h);
    final got = _blobs[key];
    if (got != null) return got;
    if (fetch && !_failedRecently(key) && _inflight.add('b:$key')) {
      _fetchBlob(h, key);
    }
    return null;
  }

  bool _failedRecently(String key) {
    final e = _blobErr[key];
    if (e == null) return false;
    if (DateTime.now().difference(e.at) < blobRetryAfter) return true;
    _blobErr.remove(key);
    return false;
  }

  /// Why the item's blob could not be fetched, while that failure stands; null otherwise.
  String? blobError(Item? item) {
    final h = item?.blobHash;
    if (item == null || item.body != null || h == null) return null;
    return _blobErr[_hex(h)]?.text;
  }

  /// Forgets the item's failed fetch; the next draw asks again.
  void retryBlob(Item? item) {
    final h = item?.blobHash;
    if (h == null || _blobErr.remove(_hex(h)) == null) return;
    _changed();
  }

  Future<void> _fetchBlob(List<int> h, String key) async {
    try {
      if (!link.isOnline) throw const LinkLost('offline');
      final r = await link.req(Req.blobGet(h), resend: false);
      _blobs[key] = utf8.decode(r.blob, allowMalformed: true);
      _blobErr.remove(key);
    } catch (e) {
      _blobErr[key] = (text: errorText(e), at: DateTime.now());
    } finally {
      _inflight.remove('b:$key');
      _changed();
    }
  }

  /// The body decoded (from [bodyText]); null when absent or cut.
  Json? json(Item item, {bool fetch = true}) {
    if (item.body != null) return item.bodyJson;
    final t = bodyText(item, fetch: fetch);
    if (t == null) return null;
    try {
      final v = jsonDecode(t);
      return v is Json ? v : null;
    } on FormatException {
      return null;
    }
  }

  /// Text of a text kind: streaming text, inline body, fetched blob, else the preview.
  String text(Item item, String Function(Item) streamText) {
    if (item.body != null || item.blobHash == null) return streamText(item);
    final t = bodyText(item);
    if (t == null) return streamText(item);
    final j = json(item);
    return j?['text'] as String? ?? stringField(t, 'text') ?? item.preview;
  }

  static String _hex(List<int> h) =>
      h.map((b) => b.toRadixString(16).padLeft(2, '0')).join();

  // ---- pictures ----

  /// The bytes of a file on the box (an `Attached:` path), fetched once and kept in memory.
  Uint8List? picture(String path) {
    final key = '$sid\n$path';
    final got = _pics[key];
    if (got != null) return got;
    if (!_picErr.containsKey(key) && _inflight.add('p:$key')) {
      _fetchPicture(path, key);
    }
    return null;
  }

  String? pictureError(String path) => _picErr['$sid\n$path'];

  Future<void> _fetchPicture(String path, String key) async {
    try {
      if (!link.isOnline) return;
      final r = await link.req(Req.fsRead(sid, path), resend: false);
      _pics[key] = Uint8List.fromList(r.fsData);
      while (_pics.length > _picCap) {
        _pics.remove(_pics.keys.first);
      }
    } catch (e) {
      _picErr[key] = errorText(e);
    } finally {
      _inflight.remove('p:$key');
      _changed();
    }
  }

  /// Forgets a failed picture so the next draw asks again.
  void retryPicture(String path) {
    _picErr.remove('$sid\n$path');
    _changed();
  }

  /// Fetches a file's bytes now (for the lightbox of a file not on screen yet).
  Future<Uint8List> readFile(String path) async {
    final have = picture(path);
    if (have != null) return have;
    final r = await link.req(Req.fsRead(sid, path), resend: false);
    return Uint8List.fromList(r.fsData);
  }

  // ---- commands ----

  /// The daemon's `/` commands, asked once per 30 s while empty or unknown.
  List<String> get commands {
    final c = _commands;
    final stale =
        _commandsAt == null ||
        ((c == null || c.isEmpty) &&
            DateTime.now().difference(_commandsAt!) >
                const Duration(seconds: 30));
    if (stale && link.isOnline && _inflight.add('cmd')) _fetchCommands();
    return c ?? const [];
  }

  Future<void> _fetchCommands() async {
    _commandsAt = DateTime.now();
    try {
      final r = await link.req(Req.commands, resend: false);
      _commands = [
        for (final n in r.commands) n.startsWith('/') ? n.substring(1) : n,
      ];
    } catch (_) {
      // asked again after 30 s
    } finally {
      _inflight.remove('cmd');
      _changed();
    }
  }

  // ---- questions ----

  /// Sends an answer; the state shows `sending…` until the item changes, or the error.
  Future<void> answer(Item item, String requestId, Answer a) async {
    final st = answers[item.itemId] = AnswerState(sending: true);
    _changed();
    try {
      await link.req(Req.answerQuestion(sid, requestId, a), resend: false);
    } catch (e) {
      st
        ..sending = false
        ..error = errorText(e);
      _changed();
    }
  }

  // ---- simple requests ----

  /// Runs [req]; returns null or the error's text.
  Future<String?> run(Object req) async {
    try {
      await link.req(req, resend: false);
      return null;
    } catch (e) {
      return errorText(e);
    }
  }

  /// Stop right after sending (`TakeBack`): the newest prompt's text when the agent gave it up
  /// and its rows are gone, null when the turn was only interrupted.
  Future<String?> takeBack() async =>
      (await link.req(Req.takeBack(sid), resend: false)).takenBack;

  // ---- drafts ----

  static String _draftKey(String sid) => 'chat.draft.$sid';

  static Future<String> loadDraft(String sid) async {
    try {
      final sp = await SharedPreferences.getInstance();
      return sp.getString(_draftKey(sid)) ?? '';
    } catch (_) {
      return '';
    }
  }

  static Future<void> saveDraft(String sid, String text) async {
    try {
      final sp = await SharedPreferences.getInstance();
      if (text.isEmpty) {
        await sp.remove(_draftKey(sid));
      } else {
        await sp.setString(_draftKey(sid), text);
      }
    } catch (_) {
      // a draft is a convenience
    }
  }

  /// Test hook: drops the static caches.
  @visibleForTesting
  static void resetCaches() {
    _pics.clear();
    _picErr.clear();
    _blobs.clear();
    _blobErr.clear();
    _commands = null;
    _commandsAt = null;
  }
}
