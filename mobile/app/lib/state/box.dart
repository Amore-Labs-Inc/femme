/// The Box tab's data, as plain Dart: the board's JSON (`/api/accounts`, `/api/keys`,
/// `/api/meta`) read the way the desktop's `accounts/model.rs` reads it, and the PR row marks of
/// `desktop/src/prs.rs`. Nothing here throws on odd input: a missing field is an absent one.
library;

import 'dart:convert';

import '../net/rpc.dart';

/// The lowest and highest per-account rotation caps, in percent.
const capMin = 50;
const capMax = 99;

/// What a failed request says on a card; `Unsupported` means the daemon predates the request.
String describeError(Object e) {
  if (e is FemmeError) {
    if (e.code == 'Unsupported') return 'box too old for this';
    return e.msg.isEmpty ? e.code : e.msg;
  }
  return '$e';
}

bool isUnsupported(Object e) => e is FemmeError && e.code == 'Unsupported';

String? _text(Object? m, String key) {
  if (m is! Map) return null;
  final v = m[key];
  return v is String && v.isNotEmpty ? v : null;
}

Map? _obj(Object? m, String key) {
  if (m is! Map) return null;
  final v = m[key];
  return v is Map ? v : null;
}

DateTime? _iso(String? s) => s == null ? null : DateTime.tryParse(s);

/// One usage window (5-hour, 7-day, or a model's own weekly one).
class Usage {
  const Usage(this.pct, this.resetsAt, this.countdown, {this.name});
  final double pct;
  final DateTime? resetsAt;
  final String? countdown;

  /// The model's name for a scoped window (`Fable`).
  final String? name;

  static Usage? fromJson(Object? w, {String? name}) {
    if (w is! Map || w['pct'] is! num) return null;
    return Usage(
      (w['pct'] as num).toDouble(),
      _iso(_text(w, 'resetsAt')),
      _text(w, 'countdown'),
      name: name,
    );
  }

  double get fill => (pct / 100).clamp(0.0, 1.0);

  /// `71% · 2d 5h`: the percentage and the time left to the reset, from [now].
  String text(DateTime now) {
    final p = (pct + 0.5).floor();
    final at = resetsAt;
    final left = at != null ? countdownText(at.difference(now)) : countdown;
    return left == null ? '$p%' : '$p% · $left';
  }
}

/// `2d 5h`, `3h 10m`, `12m`; `0m` once past.
String countdownText(Duration d) {
  if (d.isNegative) return '0m';
  if (d.inDays > 0) return '${d.inDays}d ${d.inHours % 24}h';
  if (d.inHours > 0) return '${d.inHours}h ${d.inMinutes % 60}m';
  return '${d.inMinutes}m';
}

enum Level { warn, bad }

class Account {
  const Account({
    required this.number,
    required this.title,
    this.active = false,
    this.disabled = false,
    this.fiveHour,
    this.sevenDay,
    this.scoped = const [],
    this.reason,
    this.stale = false,
    this.loginEnds,
  });
  final int number;
  final String title;
  final bool active;
  final bool disabled;
  final Usage? fiveHour;
  final Usage? sevenDay;
  final List<Usage> scoped;

  /// Why there is no fresh usage, in words.
  final String? reason;

  /// The windows are the last good ones, not the latest reading.
  final bool stale;
  final DateTime? loginEnds;

  static Account? fromJson(Object? a) {
    if (a is! Map || a['number'] is! int) return null;
    final number = a['number'] as int;
    final fresh = _obj(a, 'usage');
    final usage = fresh ?? _obj(a, 'lastGoodUsage');
    final scoped = usage?['scoped'];
    return Account(
      number: number,
      title: _text(a, 'alias') ?? _text(a, 'email') ?? 'account $number',
      active: a['active'] == true,
      disabled: a['disabled'] == true,
      fiveHour: Usage.fromJson(usage?['fiveHour']),
      sevenDay: Usage.fromJson(usage?['sevenDay']),
      scoped: [
        if (scoped is List)
          for (final s in scoped)
            ?Usage.fromJson(s, name: _text(s, 'name') ?? 'model'),
      ],
      reason: fresh != null
          ? null
          : _reason(_text(a, 'usageStatus'), _text(a, 'usageError')),
      stale: fresh == null && usage != null,
      loginEnds: _iso(_text(a, 'loginExpiresAt')),
    );
  }

  static String _reason(String? status, String? error) => switch (status) {
    'token_expired' => 'token expired',
    'relogin_required' => 're-login needed',
    'api_key' => 'api key: no usage quota',
    'keychain_unavailable' => 'keychain unavailable',
    'foreign_credential' => 'foreign credential',
    'no_credentials' => 'no credentials',
    _ => error == null ? 'usage unknown' : 'usage unavailable ($error)',
  };

  /// The sign-in line and how loud it is: nothing with more than 10 days left.
  (String, Level)? signIn(DateTime now) {
    final end = loginEnds;
    if (end == null) return null;
    final left = end.difference(now);
    if (left > const Duration(days: 10)) return null;
    if (left.isNegative) return ('sign-in ended', Level.bad);
    final level = left < const Duration(days: 2) ? Level.bad : Level.warn;
    final words = left.inDays >= 2 ? '${left.inDays} d' : '${left.inHours} h';
    return ('sign-in ends in $words', level);
  }
}

/// `GET /api/accounts`: the accounts, or the error cswap gave (null list = not read yet).
({List<Account>? accounts, String? error}) parseAccounts(String body) {
  final Object? v;
  try {
    v = jsonDecode(body);
  } on FormatException catch (e) {
    return (accounts: null, error: 'not JSON: ${e.message}');
  }
  // The route answers `{cswap, aws, login}`; the desktop's fixture nests it under `accounts`.
  final root = v is Map && v['cswap'] == null && v['accounts'] is Map
      ? v['accounts']
      : v;
  if (root is! Map) return (accounts: null, error: 'not an object');
  final c = root['cswap'];
  if (c == null) return (accounts: null, error: null);
  if (c is Map && c['error'] != null) {
    final e = c['error'];
    return (
      accounts: null,
      error: e is String ? e : (_text(e, 'message') ?? 'cswap failed'),
    );
  }
  final list = c is Map ? c['accounts'] : null;
  return (
    accounts: [
      if (list is List)
        for (final a in list) ?Account.fromJson(a),
    ],
    error: null,
  );
}

class ApiKey {
  const ApiKey(this.name, this.hint, this.account, this.created, this.error);
  final String name;
  final String hint;
  final String account;
  final DateTime? created;
  final String? error;

  static ApiKey? fromJson(Object? k) {
    final name = _text(k, 'name');
    if (name == null) return null;
    final c = (k as Map)['created'];
    return ApiKey(
      name,
      _text(k, 'hint') ?? '',
      _text(k, 'account') ?? 'the box',
      c is int ? DateTime.fromMillisecondsSinceEpoch(c * 1000) : null,
      _text(k, 'error'),
    );
  }
}

/// `GET /api/keys` (and the answers of revoke): a list, or an object holding one as `keys`.
List<ApiKey> parseKeys(String body) {
  final Object? v;
  try {
    v = jsonDecode(body);
  } on FormatException {
    return const [];
  }
  final list = v is Map ? v['keys'] : v;
  return [
    if (list is List)
      for (final k in list) ?ApiKey.fromJson(k),
  ];
}

/// `POST /api/keys`: the secret, shown once.
String? parseNewKeySecret(String body) {
  try {
    return _text(jsonDecode(body), 'key');
  } on FormatException {
    return null;
  }
}

/// `GET /api/meta`: the box's default rotation cap.
int? parseMetaCap(String body) {
  try {
    final v = jsonDecode(body);
    final cap = v is Map ? v['cap'] : null;
    return cap is int ? cap : null;
  } on FormatException {
    return null;
  }
}

/// A key name the board accepts.
bool validKeyName(String s) => RegExp(r'^[a-z0-9-]{1,32}$').hasMatch(s);

/// The mark a PR row carries next to its title.
enum PrBadge { merging, fixing, conflicts, checking }

/// The row's mark for [pr]; none when it merges cleanly.
PrBadge? prBadge(Pr pr) {
  if (pr.merging) return PrBadge.merging;
  if (pr.fixing != null) return PrBadge.fixing;
  return switch (pr.mergeable) {
    false => PrBadge.conflicts,
    null => PrBadge.checking,
    true => null,
  };
}

String prBadgeLabel(PrBadge b) => switch (b) {
  PrBadge.merging => 'merging…',
  PrBadge.fixing => 'fixing…',
  PrBadge.conflicts => 'conflicts',
  PrBadge.checking => 'checking',
};

/// Whether the list is worth reading again soon.
bool prsUnsettled(List<Pr> prs) =>
    prs.any((p) => p.merging || p.fixing != null || p.mergeable == null);

/// A board call's answer, or throws a [FemmeError] for a non-2xx status.
Future<String> boardCall(
  Future<Resp> Function(Object req) req,
  String method,
  String path, {
  Object? body,
}) async {
  final r = (await req(
    Req.boardApi(method, path, body: body == null ? '' : jsonEncode(body)),
  )).boardApi;
  if (r.status < 200 || r.status >= 300) {
    String msg = r.body;
    try {
      final v = jsonDecode(r.body);
      msg = _text(v, 'error') ?? _text(v, 'message') ?? r.body;
    } on FormatException {
      // keep the raw body
    }
    throw FemmeError('Http${r.status}', msg.isEmpty ? 'HTTP ${r.status}' : msg);
  }
  return r.body;
}
