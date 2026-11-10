import 'dart:async';

import 'package:flutter/cupertino.dart';

import '../net/models.dart' hide Row;
import '../net/rpc.dart';
import '../state/store.dart';
import 'femme.dart';
import 'new_chat.dart';
import 'theme.dart';
import 'widgets.dart';

/// The Now sections in order, each with its sessions (empty ones included).
List<(String, List<Session>)> nowSections(Store s) => [
  ('Needs you', s.needsYou),
  ('Done', s.done),
  ('Working', s.working),
  ('Idle', s.idle),
];

/// The open question of a transcript: the newest `Question` item still `Pending`, unanswered.
Question? pendingQuestion(List<Item> items) {
  for (final i in items.reversed) {
    if (i.kind != ItemKind.question || i.status != ItemStatus.pending) continue;
    final q = i.question;
    if (q != null && q.answered == null && q.reason == null) return q;
  }
  return null;
}

/// The newest item of one of [kinds], as one line (its last non-empty line).
String? lastLine(List<Item> items, Set<ItemKind> kinds, {Store? store}) {
  for (final i in items.reversed) {
    if (!kinds.contains(i.kind)) continue;
    final text = i.kind == ItemKind.assistantMessage
        ? (store?.textOf(i) ?? i.text).trim()
        : i.preview.trim();
    final lines = (text.isEmpty ? i.preview : text)
        .split('\n')
        .map((l) => l.trim())
        .where((l) => l.isNotEmpty);
    if (lines.isNotEmpty) return lines.last;
  }
  return null;
}

const _toolKinds = {
  ItemKind.toolCall,
  ItemKind.assistantMessage,
  ItemKind.event,
  ItemKind.thinking,
};

/// How many sessions the tab keeps subscribed for their question / preview.
const _maxHeld = 12;

/// The home tab: what needs you, what is done, what works, what idles.
class NowTab extends StatefulWidget {
  const NowTab({super.key});

  @override
  State<NowTab> createState() => _NowTabState();
}

class _NowTabState extends State<NowTab> {
  final _held = <String>{};
  final _answering = <String>{};
  Timer? _tick;
  Store? _store;

  /// When this app first saw a session working, for the elapsed time before its items load.
  static final _workingSince = <String, DateTime>{};

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final f = Femme.of(context);
    _store = f.store;
    final visible = TickerMode.valuesOf(context).enabled;
    final want = <String>{};
    if (visible) {
      for (final s in [
        ...f.store.needsYou,
        ...f.store.done,
        ...f.store.working,
      ]) {
        if (want.length >= _maxHeld) break;
        want.add(s.id);
      }
    }
    for (final sid in want.difference(_held)) {
      f.store.subscribe(sid);
    }
    for (final sid in _held.difference(want)) {
      f.store.unsubscribe(sid);
    }
    _held
      ..clear()
      ..addAll(want);

    final working = f.store.working;
    final now = DateTime.now();
    for (final s in working) {
      _workingSince.putIfAbsent(s.id, () => now);
    }
    _workingSince.removeWhere((k, _) => !working.any((s) => s.id == k));
    if (working.isNotEmpty && visible) {
      _tick ??= Timer.periodic(
        const Duration(seconds: 1),
        (_) => mounted ? setState(() {}) : null,
      );
    } else {
      _tick?.cancel();
      _tick = null;
    }
  }

  @override
  void dispose() {
    _tick?.cancel();
    for (final sid in _held) {
      _store?.unsubscribe(sid);
    }
    super.dispose();
  }

  Future<void> _refresh() async {
    final link = Femme.of(context).link;
    if (!link.isOnline) return;
    try {
      await link.req(Req.subscribe(Scope.global), resend: false);
    } catch (_) {
      // the list stays as it is
    }
  }

  Future<void> _answer(Session s, Question q, int i) async {
    final link = Femme.of(context).link;
    setState(() => _answering.add(s.id));
    actionHaptic();
    try {
      await link.req(
        Req.answerQuestion(
          s.id,
          q.requestId,
          q.multi ? Answer.options([i]) : Answer.option(i),
        ),
      );
    } catch (e) {
      if (mounted) await showProblem(context, 'Answer not sent', e);
    } finally {
      if (mounted) setState(() => _answering.remove(s.id));
    }
  }

  Future<void> _ok(Session s) async {
    actionHaptic();
    try {
      await Femme.of(context).store.send(s.id, 'ok');
    } catch (e) {
      if (mounted) await showProblem(context, 'Not sent', e);
    }
  }

  Duration _elapsed(Session s, List<Item> items) {
    for (final i in items.reversed) {
      if (i.kind == ItemKind.turnStart) {
        return DateTime.now().difference(
          DateTime.fromMillisecondsSinceEpoch(i.ts),
        );
      }
    }
    final since = _workingSince[s.id];
    return since == null ? Duration.zero : DateTime.now().difference(since);
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final store = f.store;
    final link = f.link.state.value;
    final sections = nowSections(store);
    final nothingWaits = sections[0].$2.isEmpty;
    final any = sections.any((x) => x.$2.isNotEmpty);

    Widget row(Session s, Widget body) => SessionActions(
      session: s,
      child: GestureDetector(
        behavior: HitTestBehavior.opaque,
        onTap: () {
          tapHaptic();
          pushChat(context, s.id);
        },
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 10, 16, 10),
          child: body,
        ),
      ),
    );

    Widget head(Session s, String trailing) => Row(
      children: [
        StatusDot(statusColor(s)),
        const SizedBox(width: 10),
        Expanded(
          child: Text(
            s.title,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: const TextStyle(fontWeight: FontWeight.w600),
          ),
        ),
        Text(trailing, style: _meta(context)),
      ],
    );

    Widget sub(String? text, {int lines = 1}) => text == null
        ? const SizedBox.shrink()
        : Padding(
            padding: const EdgeInsets.only(top: 4, left: 18),
            child: Text(
              text,
              maxLines: lines,
              overflow: TextOverflow.ellipsis,
              style: _meta(context).copyWith(fontSize: 15),
            ),
          );

    Widget needsCard(Session s) {
      final q = pendingQuestion(store.items(s.id));
      final busy = _answering.contains(s.id);
      return row(
        s,
        Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            head(s, 'needs you'),
            if (q == null)
              sub(store.isSubscribed(s.id) ? 'waiting on you' : 'loading…')
            else ...[
              Padding(
                padding: const EdgeInsets.only(top: 6, left: 18),
                child: Text(q.prompt, maxLines: 6),
              ),
              for (final (i, o) in q.options.take(3).indexed)
                Padding(
                  padding: const EdgeInsets.only(top: 8, left: 18),
                  child: CupertinoButton.tinted(
                    sizeStyle: CupertinoButtonSize.medium,
                    onPressed: busy ? null : () => _answer(s, q, i),
                    child: Text(o, maxLines: 2, textAlign: TextAlign.center),
                  ),
                ),
              if (q.options.length > 3 || q.freeText)
                Padding(
                  padding: const EdgeInsets.only(top: 4, left: 18),
                  child: CupertinoButton(
                    sizeStyle: CupertinoButtonSize.small,
                    onPressed: () => pushChat(context, s.id),
                    child: const Text('Other…'),
                  ),
                ),
            ],
          ],
        ),
      );
    }

    Widget doneRow(Session s) {
      final items = store.items(s.id);
      return row(
        s,
        Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            head(s, 'done'),
            Row(
              children: [
                Expanded(
                  child: sub(
                    lastLine(items, const {
                      ItemKind.assistantMessage,
                    }, store: store),
                    lines: 2,
                  ),
                ),
                CupertinoButton(
                  sizeStyle: CupertinoButtonSize.small,
                  onPressed: () => _ok(s),
                  child: const Text('👍 ok'),
                ),
              ],
            ),
          ],
        ),
      );
    }

    Widget workingRow(Session s) {
      final items = store.items(s.id);
      return row(
        s,
        Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            head(s, elapsed(_elapsed(s, items))),
            sub(lastLine(items, _toolKinds, store: store) ?? 'working…'),
          ],
        ),
      );
    }

    Widget idleRow(Session s) => row(
      s,
      Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [head(s, s.status.name), sub(homeShort(s.cwd))],
      ),
    );

    return TabPage(
      title: 'Now',
      status: linkLine(link),
      dot: linkColor(link),
      onRefresh: _refresh,
      trailing: CupertinoButton(
        padding: EdgeInsets.zero,
        onPressed: () => openNewChat(context),
        child: const Icon(CupertinoIcons.add),
      ),
      slivers: [
        if (nothingWaits)
          SliverToBoxAdapter(
            child: Padding(
              padding: const EdgeInsets.fromLTRB(20, 24, 20, 8),
              child: Column(
                children: [
                  const Icon(
                    CupertinoIcons.checkmark_seal,
                    size: 40,
                    color: FemmeColors.done,
                  ),
                  const SizedBox(height: 8),
                  Text('Nothing is waiting on you.', style: _meta(context)),
                ],
              ),
            ),
          ),
        section('Needs you', [for (final s in sections[0].$2) needsCard(s)]),
        section('Done', [for (final s in sections[1].$2) doneRow(s)]),
        section('Working', [for (final s in sections[2].$2) workingRow(s)]),
        section('Idle', [for (final s in sections[3].$2) idleRow(s)]),
        if (!any)
          SliverToBoxAdapter(
            child: Center(
              child: CupertinoButton.filled(
                onPressed: () => openNewChat(context),
                child: const Text('+ chat'),
              ),
            ),
          ),
      ],
    );
  }
}

TextStyle _meta(BuildContext context) =>
    TextStyle(fontSize: 13, color: FemmeColors.mutedC.resolveFrom(context));
