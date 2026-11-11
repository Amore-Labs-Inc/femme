/// Push through ntfy: this phone owns one ntfy topic (a random secret name), the box posts to
/// it when a session needs you or finishes (`Req::PushRegister`, see `mobile/PROTOCOL.md`), and
/// the ntfy app on the phone shows it on the lock screen. No Apple or Google account needed.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';

import '../net/link.dart';
import '../net/rpc.dart';

/// What the box may push to this phone.
class PushRules {
  const PushRules({
    this.blocked = true,
    this.done = true,
    this.quiet,
    this.muted = const {},
  });

  /// "needs you": a question or a permission prompt waits.
  final bool blocked;

  /// "finished": a session went idle after working.
  final bool done;

  /// Local hours `[from, to)` with no pushes, e.g. `(23, 7)`; wraps midnight.
  final (int, int)? quiet;

  /// Sessions never pushed.
  final Set<String> muted;

  PushRules copyWith({
    bool? blocked,
    bool? done,
    (int, int)? quiet,
    bool clearQuiet = false,
    Set<String>? muted,
  }) => PushRules(
    blocked: blocked ?? this.blocked,
    done: done ?? this.done,
    quiet: clearQuiet ? null : (quiet ?? this.quiet),
    muted: muted ?? this.muted,
  );

  /// The wire shape of `PushRules` (serde: a tuple is a JSON array).
  Map<String, Object?> toJson() => {
    'blocked': blocked,
    'done': done,
    'muted': muted.toList()..sort(),
    'quiet': quiet == null ? null : [quiet!.$1, quiet!.$2],
  };

  static PushRules fromJson(Object? j) {
    if (j is! Map) return const PushRules();
    final q = j['quiet'];
    return PushRules(
      blocked: j['blocked'] != false,
      done: j['done'] != false,
      quiet: q is List && q.length == 2
          ? ((q[0] as num).toInt(), (q[1] as num).toInt())
          : null,
      muted: {
        for (final m in (j['muted'] as List?) ?? const [])
          if (m is String) m,
      },
    );
  }

  @override
  bool operator ==(Object other) =>
      other is PushRules && jsonEncode(toJson()) == jsonEncode(other.toJson());

  @override
  int get hashCode => jsonEncode(toJson()).hashCode;
}

/// A fresh topic name: `femme-` and 20 random base32 characters (100 bits; the name is the
/// secret, as with every ntfy topic).
String newTopic([Random? rng]) {
  const alphabet = 'abcdefghijklmnopqrstuvwxyz234567';
  final r = rng ?? Random.secure();
  return 'femme-${List.generate(20, (_) => alphabet[r.nextInt(32)]).join()}';
}

/// Where the phone keeps its push settings (the [Prefs]).
abstract interface class PushKeeper {
  String? get pushTopic;
  bool get pushOn;
  String? get pushRulesJson;
  Future<void> setPush({String? topic, bool? on, String? rulesJson});
}

/// Keeps the box's registration in step with the settings: registers when the link comes up
/// and whenever the settings change, unregisters when push is turned off. A failed request is
/// kept as [error] (shown on the Me tab) and tried again on the next connection.
class PushSync extends ChangeNotifier {
  PushSync(this._req, this._link, this._keeper) {
    _link.addListener(_onLink);
    _onLink();
  }

  final Future<Resp> Function(Object req) _req;
  final ValueListenable<LinkState> _link;
  final PushKeeper _keeper;

  /// The topic the box last confirmed, or null.
  String? registered;

  /// Why the last request failed, or null; `Unsupported` means the box predates push.
  String? error;

  bool get unsupported => error == 'Unsupported';
  bool _busy = false;
  bool _again = false;

  bool get on => _keeper.pushOn;
  String? get topic => _keeper.pushTopic;
  PushRules get rules => PushRules.fromJson(
    _keeper.pushRulesJson == null ? null : jsonDecode(_keeper.pushRulesJson!),
  );

  /// Turns push on (minting a topic the first time) or off.
  Future<void> setOn(bool on) async {
    await _keeper.setPush(topic: _keeper.pushTopic ?? newTopic(), on: on);
    notifyListeners();
    await sync();
  }

  Future<void> setRules(PushRules r) async {
    await _keeper.setPush(rulesJson: jsonEncode(r.toJson()));
    notifyListeners();
    await sync();
  }

  /// A new topic: the old one stops getting pushes once the box confirms the new one.
  Future<void> rotate() async {
    await _keeper.setPush(topic: newTopic());
    notifyListeners();
    await sync();
  }

  void _onLink() {
    if (_link.value.online) sync();
  }

  /// Sends what the settings say, once at a time; a change during a request runs it again.
  Future<void> sync() async {
    if (_busy) {
      _again = true;
      return;
    }
    if (!_link.value.online) return;
    _busy = true;
    try {
      final topic = _keeper.pushTopic;
      if (_keeper.pushOn && topic != null) {
        final want = (topic, rules);
        if (_sent == want) return;
        await _req(Req.pushRegister(topic, want.$2));
        _sent = want;
        registered = topic;
      } else if (registered != null) {
        await _req(Req.pushUnregister(registered!));
        registered = null;
        _sent = null;
      }
      error = null;
    } on FemmeError catch (e) {
      error = e.code == 'Unsupported' ? 'Unsupported' : e.msg;
    } catch (e) {
      error = '$e';
    } finally {
      _busy = false;
      notifyListeners();
      if (_again) {
        _again = false;
        sync().ignore();
      }
    }
  }

  (String, PushRules)? _sent;

  @override
  void dispose() {
    _link.removeListener(_onLink);
    super.dispose();
  }
}
