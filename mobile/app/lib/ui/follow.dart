/// The desktop's "current session follows" rules (`desktop/src/net.rs` `keep_current` and
/// `follow_claude`) for the session screen on show: when the shown session closes, the screen
/// moves to the newest live session (or back to the list when none is live); a Claude session
/// that appears while a live shell is on show (`claude` typed in it) takes its place.
library;

import 'package:flutter/cupertino.dart';

import '../net/models.dart';
import '../state/store.dart';
import 'chat/look.dart';
import 'femme.dart';
import 'widgets.dart';

/// Whether [s] is live (listed, not closed).
bool _live(Session s) => s.closed == null && s.status != SessionStatus.closed;

/// Newest live session by (created, id), or null (desktop `Model::live().first()`).
String? newestLive(Store store) {
  Session? best;
  for (final s in store.sessions.values) {
    if (!_live(s)) continue;
    if (best == null ||
        s.created > best.created ||
        (s.created == best.created && s.id.compareTo(best.id) > 0)) {
      best = s;
    }
  }
  return best?.id;
}

/// The Claude session among [arrived] (sids new to the store since the last look) that takes the
/// place of [shown] when [shown] is a live shell: the newest running one, else null.
String? claudeFor(Store store, String shown, Iterable<String> arrived) {
  final cur = store.sessions[shown];
  if (cur == null || cur.agent != Agent.shell || !_live(cur)) return null;
  Session? best;
  for (final sid in arrived) {
    final s = store.sessions[sid];
    if (s == null || s.agent != Agent.claude || !_live(s)) continue;
    if (s.status == SessionStatus.crashed || s.status == SessionStatus.failed) {
      continue;
    }
    if (best == null ||
        s.created > best.created ||
        (s.created == best.created && s.id.compareTo(best.id) > 0)) {
      best = s;
    }
  }
  return best?.id;
}

/// Wraps the screen of [sid] and replaces it when the desktop would move the centre. A session
/// opened after it had already closed stays (the `session closed` card is what was asked for).
class Following extends StatefulWidget {
  const Following({super.key, required this.sid, required this.child});

  final String sid;
  final Widget child;

  @override
  State<Following> createState() => _FollowingState();
}

class _FollowingState extends State<Following> {
  bool _sawLive = false;
  bool _moved = false;
  Set<String>? _known;

  @override
  Widget build(BuildContext context) {
    final store = Femme.of(context).store;
    final s = store.sessions[widget.sid];
    if (s != null && _live(s)) _sawLive = true;
    final arrived = _known == null
        ? const <String>[]
        : store.sessions.keys.where((k) => !_known!.contains(k)).toList();
    _known = store.sessions.keys.toSet();

    if (!_moved) {
      String? to;
      String? why;
      if (_sawLive && store.isClosed(widget.sid)) {
        to = newestLive(store);
        why = '${store.session(widget.sid)?.title ?? 'session'} closed';
      } else if (arrived.isNotEmpty) {
        to = claudeFor(store, widget.sid, arrived);
      }
      if (why != null || to != null) {
        _moved = true;
        final target = to;
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (!mounted) return;
          final nav = Navigator.of(context);
          if (target == null) {
            nav.maybePop();
          } else {
            nav.pushReplacement(
              CupertinoPageRoute<void>(
                builder: (_) => sessionScreen(store.session(target), target),
              ),
            );
          }
          if (why != null) toast(context, why);
        });
      }
    }
    return widget.child;
  }
}
