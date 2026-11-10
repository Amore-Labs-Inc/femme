/// The Box tab's two sign-ins and its scheduler line, as plain Dart: a Claude account added to
/// the box (`/api/accounts/claude/...`), a GitHub account connected to an API key with GitHub's
/// device code (`/api/keys/{name}/github`), and the footer `/api/meta` gives. Read and driven the
/// way the desktop's `accounts/{api,flow,model,view}.rs` do; the flows talk to the board through a
/// request function, so tests hand them a fake one.
library;

import 'dart:convert';

import 'package:flutter/foundation.dart';

import '../net/rpc.dart';
import 'box.dart';

/// Sends one request to the daemon (`Femme.of(context).link.req`, or a test's fake).
typedef BoardReq = Future<Resp> Function(Object req);

Object? _json(String body) {
  try {
    return jsonDecode(body);
  } on FormatException {
    return null;
  }
}

String? _text(Object? m, String key) {
  if (m is! Map) return null;
  final v = m[key];
  return v is String && v.isNotEmpty ? v : null;
}

// ---- Claude sign-in ----------------------------------------------------------------------------

/// The link of the Claude sign-in in progress (`login.claude` of `/api/accounts` and of every
/// account action's answer), or null when none is.
String? parseLoginLink(String body) {
  final v = _json(body);
  final root = v is Map && v['cswap'] == null && v['accounts'] is Map
      ? v['accounts']
      : v;
  final login = root is Map ? root['login'] : null;
  return _text(login, 'claude');
}

enum LoginPhase {
  /// Nothing asked yet.
  idle,

  /// `POST /api/accounts/claude/login` is out.
  starting,

  /// Started, but the box has no link yet: [LoginFlow.refresh] asks again.
  waitingLink,

  /// The link is known; the code goes in.
  link,

  /// The code is on its way.
  sending,

  /// The sign-in ended without the account (the code was refused and the box let it go).
  ended,

  /// The account was added.
  done,

  /// Given up.
  cancelled,
}

/// "+ add": a Claude sign-in on the box. Start it, open its link, paste the code the page shows.
class LoginFlow extends ChangeNotifier {
  /// [link]: a sign-in already in progress, picked up where it is.
  LoginFlow(this._req, {String? link})
    : link = link,
      phase = link == null ? LoginPhase.idle : LoginPhase.link;

  final BoardReq _req;
  LoginPhase phase;
  String? link;

  /// The last refusal, in words.
  String? error;
  bool _gone = false;

  bool get busy => phase == LoginPhase.starting || phase == LoginPhase.sending;

  void _set(LoginPhase p, {String? link, String? error}) {
    phase = p;
    if (link != null) this.link = link;
    this.error = error;
    if (!_gone) notifyListeners();
  }

  /// `POST /api/accounts/claude/login {}`; its answer carries the link.
  Future<void> start() async {
    if (busy) return;
    _set(LoginPhase.starting);
    try {
      final body = await boardCall(
        _req,
        'POST',
        '/api/accounts/claude/login',
        body: const {},
      );
      final url = parseLoginLink(body);
      _set(url == null ? LoginPhase.waitingLink : LoginPhase.link, link: url);
    } catch (e) {
      _set(LoginPhase.ended, error: describeError(e));
    }
  }

  /// Reads `/api/accounts` for a link that was not there yet.
  Future<void> refresh() async {
    if (phase != LoginPhase.waitingLink) return;
    try {
      final url = parseLoginLink(await boardCall(_req, 'GET', '/api/accounts'));
      if (url != null && phase == LoginPhase.waitingLink) {
        _set(LoginPhase.link, link: url);
      }
    } catch (_) {
      // asked again on the next tick
    }
  }

  /// "Add account": `POST /api/accounts/claude/code {"code": trimmed}`. Whether it was added.
  Future<bool> submit(String code) async {
    if (busy || phase != LoginPhase.link) return false;
    final c = code.trim();
    if (c.isEmpty) {
      _set(phase, error: 'paste the code the sign-in page shows');
      return false;
    }
    _set(LoginPhase.sending);
    try {
      await boardCall(
        _req,
        'POST',
        '/api/accounts/claude/code',
        body: {'code': c},
      );
      _set(LoginPhase.done);
      return true;
    } catch (e) {
      final why = describeError(e);
      // A refused code can end the sign-in on the box: it is still on only if the link is.
      String? still;
      try {
        still = parseLoginLink(await boardCall(_req, 'GET', '/api/accounts'));
      } catch (_) {
        still = link;
      }
      _set(
        still == null ? LoginPhase.ended : LoginPhase.link,
        link: still,
        error: why,
      );
      return false;
    }
  }

  /// "Cancel": `DELETE /api/accounts/claude/login`.
  Future<void> cancel() async {
    final started = phase != LoginPhase.idle && phase != LoginPhase.done;
    _set(LoginPhase.cancelled);
    if (!started) return;
    try {
      await boardCall(_req, 'DELETE', '/api/accounts/claude/login');
    } catch (_) {
      // the box drops it by itself once its link expires
    }
  }

  @override
  void dispose() {
    _gone = true;
    super.dispose();
  }
}
