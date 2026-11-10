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

/// A key name as a path segment, percent-encoded like the desktop's `api::segment`: only
/// `[A-Za-z0-9-._~]` stays as is, so a listed name cannot leave its segment.
String pathSegment(String s) {
  final out = StringBuffer();
  for (final b in utf8.encode(s)) {
    final c = String.fromCharCode(b);
    if (RegExp(r'^[A-Za-z0-9\-._~]$').hasMatch(c)) {
      out.write(c);
    } else {
      out.write('%${b.toRadixString(16).toUpperCase().padLeft(2, '0')}');
    }
  }
  return out.toString();
}

// ---- /api/meta ---------------------------------------------------------------------------------

/// What `/api/meta` says that the Accounts section uses.
class BoxMeta {
  const BoxMeta({this.cap, this.state});

  /// The box's default rotation cap, in percent.
  final int? cap;

  /// The scheduler's word, `working (31% used, pause at 60%)`.
  final String? state;
}

/// `GET /api/meta`; what is not there is null.
BoxMeta parseMeta(String body) {
  final v = _json(body);
  final cap = v is Map ? v['cap'] : null;
  return BoxMeta(
    cap: cap is int && cap >= 0 && cap <= 255 ? cap : null,
    state: _text(v, 'state'),
  );
}

/// The footer: where the box moves off an account, and what its scheduler says (desktop
/// `accounts/view.rs` `usage_line`, word for word).
String schedulerLine(BoxMeta meta) {
  final cap = meta.cap;
  return [
    cap == null ? 'switches account at its cap' : 'switches account at $cap%',
    'agents pause when every account is past it',
    ?meta.state,
  ].join(' · ');
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

// ---- GitHub device sign-in for a key -----------------------------------------------------------

/// A GitHub sign-in in progress for a key: the code to enter and where.
class GithubDevice {
  const GithubDevice(this.code, this.url);
  final String code;
  final String url;
}

/// A key's GitHub side, from `/api/keys` (and the answers of the GitHub calls).
class KeyGithub {
  const KeyGithub(this.name, this.account, this.device, this.error);
  final String name;

  /// Whose GitHub account its clones push as: `the box`, `its own token` or `Name (login)`.
  final String account;
  final GithubDevice? device;

  /// How the last GitHub sign-in failed.
  final String? error;
}

/// Every key's GitHub side, by name. `GET /api/keys` answers a list, or an object holding one as
/// `keys`.
Map<String, KeyGithub> parseKeysGithub(String body) {
  final v = _json(body);
  final list = v is Map ? v['keys'] : v;
  final out = <String, KeyGithub>{};
  if (list is! List) return out;
  for (final k in list) {
    final name = _text(k, 'name');
    if (name == null) continue;
    final g = (k as Map)['github'];
    final code = _text(g, 'code');
    out[name] = KeyGithub(
      name,
      _text(k, 'account') ?? 'the box',
      code == null
          ? null
          : GithubDevice(
              code,
              _text(g, 'url') ?? 'https://github.com/login/device',
            ),
      _text(k, 'error'),
    );
  }
  return out;
}

enum GithubPhase {
  /// Nothing asked yet.
  idle,

  /// `POST /api/keys/{name}/github` is out.
  starting,

  /// Started; gh has not printed its code yet.
  waiting,

  /// The code is shown; the keys are read every 3 s until it is entered.
  code,

  /// The key pushes as [GithubFlow.account] now.
  connected,

  /// The sign-in failed, or the key went.
  failed,

  /// Given up.
  cancelled,
}

/// "connect GitHub" on a key: GitHub's device code, then the keys read until the key has the
/// account (the desktop polls while a key carries `github`; when it goes, the sign-in ended).
class GithubFlow extends ChangeNotifier {
  GithubFlow(this._req, this.name);

  /// Seconds between two reads of the keys while the code waits to be entered.
  static const every = Duration(seconds: 3);

  final BoardReq _req;
  final String name;
  GithubPhase phase = GithubPhase.idle;
  GithubDevice? device;

  /// Who the key pushes as, as last read.
  String? account;
  String? error;
  String? _before;
  bool _gone = false;

  String get _path => '/api/keys/${pathSegment(name)}/github';

  /// Whether the keys should be read again.
  bool get polling => phase == GithubPhase.waiting || phase == GithubPhase.code;

  void _set(GithubPhase p, {String? error}) {
    phase = p;
    this.error = error;
    if (!_gone) notifyListeners();
  }

  /// Where a keys answer leaves the sign-in.
  void _apply(String body) {
    if (!polling && phase != GithubPhase.starting) return;
    final k = parseKeysGithub(body)[name];
    if (k == null) {
      _set(GithubPhase.failed, error: 'the key is gone');
      return;
    }
    account = k.account;
    final d = k.device;
    if (d != null) {
      device = d;
      _set(GithubPhase.code);
    } else if (k.error != null) {
      _set(GithubPhase.failed, error: k.error);
    } else if (device != null || (_before != null && k.account != _before)) {
      // The code was shown and is gone without an error: gh ended well and the box settled it.
      _set(GithubPhase.connected);
    } else {
      _set(GithubPhase.waiting);
    }
  }

  /// `POST /api/keys/{name}/github {}`; the answer is the key list with the code.
  Future<void> start({String? accountBefore}) async {
    if (phase == GithubPhase.starting) return;
    _before = accountBefore;
    device = null;
    _set(GithubPhase.starting);
    try {
      _apply(await boardCall(_req, 'POST', _path, body: const {}));
    } catch (e) {
      _set(GithubPhase.failed, error: describeError(e));
    }
  }

  /// Picks up a sign-in already in progress (the key's row showed its code).
  void resume(GithubDevice d, String account) {
    device = d;
    this.account = account;
    _set(GithubPhase.code);
  }

  /// `GET /api/keys`, while [polling].
  Future<void> poll() async {
    if (!polling) return;
    try {
      _apply(await boardCall(_req, 'GET', '/api/keys'));
    } catch (_) {
      // a missed read: the next one says
    }
  }

  /// "Cancel": `DELETE /api/keys/{name}/github`.
  Future<void> cancel() async {
    final out = polling || phase == GithubPhase.starting;
    _set(GithubPhase.cancelled);
    if (!out) return;
    try {
      await boardCall(_req, 'DELETE', _path);
    } catch (_) {
      // gh's code expires on its own
    }
  }

  @override
  void dispose() {
    _gone = true;
    super.dispose();
  }
}
