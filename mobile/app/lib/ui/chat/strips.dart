/// The thin lines around the composer: the context XP bar, the goal strip, the working line.
library;

import 'dart:async';

import 'package:flutter/cupertino.dart';

import '../../net/models.dart' hide Row;
import '../../state/chat.dart';
import '../femme.dart';
import '../theme.dart';
import 'look.dart';
import 'media.dart';
import 'turns.dart';

Future<void> _note(BuildContext context, String title, String body) =>
    showCupertinoDialog<void>(
      context: context,
      barrierDismissible: true,
      builder: (ctx) => CupertinoAlertDialog(
        title: Text(title),
        content: Text(body),
        actions: [
          CupertinoDialogAction(
            isDefaultAction: true,
            onPressed: () => Navigator.pop(ctx),
            child: const Text('OK'),
          ),
        ],
      ),
    );

/// The XP bar's tap card, label and value, in the desktop's order (xp.rs rows); `?` while the
/// window is unknown. Progress and room are measured against where the agent compacts
/// ([Context.full]), which gets its own row once the agent said it.
List<(String, String)> xpRows(Context c) {
  final known = c.full > 0;
  String or(String s) => known ? s : '?';
  final frac = c.fraction ?? 0.0;
  final level = c.compactions + 1;
  return [
    (
      'current context',
      '${grouped(c.used)} / ${c.window > 0 ? grouped(c.window) : '?'}',
    ),
    if (c.compactAt != null) ('compacts at', grouped(c.compactAt!)),
    ('level progress', or('${(frac * 100).toStringAsFixed(1)}%')),
    ('room left', or(grouped(c.full > c.used ? c.full - c.used : 0))),
    (
      'level',
      switch (c.compactions) {
        0 => '$level (never compacted)',
        1 => '$level (1 compaction)',
        final n => '$level ($n compactions)',
      },
    ),
  ];
}

/// The context XP bar (desktop composer/xp.rs): fill = used / where the agent compacts (its window
/// while it has not said; 0 while unknown), red from 90 %, a capsule cut into 5 segments by notches;
/// level = compactions + 1, and a compaction seen while the chat is open plays a neon ring once
/// around the bar.
class XpBar extends StatefulWidget {
  const XpBar(this.context_, {super.key});
  final Context? context_;

  /// Notches cut the bar into this many segments.
  static const segments = 5;

  /// How long the level-up ring plays.
  static const ring = Duration(milliseconds: 700);

  @override
  State<XpBar> createState() => _XpBarState();
}

class _XpBarState extends State<XpBar> with SingleTickerProviderStateMixin {
  late final _ring = AnimationController(vsync: this, duration: XpBar.ring);

  int? _level(XpBar w) =>
      w.context_ == null ? null : w.context_!.compactions + 1;

  @override
  void didUpdateWidget(XpBar old) {
    super.didUpdateWidget(old);
    final was = _level(old);
    final now = _level(widget);
    if (was != null && now != null && now > was) _ring.forward(from: 0);
  }

  @override
  void dispose() {
    _ring.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.context_;
    final frac = c?.fraction ?? 0;
    final level = (c?.compactions ?? 0) + 1;
    final red = frac >= 0.9;
    // notches cut the capsule in the page colour, as gaps between segments
    final rule = CupertinoColors.systemBackground.resolveFrom(context);
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: () => _note(
        context,
        'Context · level $level',
        c == null
            ? 'The agent has not reported its context yet.'
            : [for (final (k, v) in xpRows(c)) '$k  $v'].join('\n'),
      ),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 6, 16, 2),
        child: Row(
          children: [
            Text(
              'L$level',
              style: mono(context, size: 11, color: muted(context)),
            ),
            const SizedBox(width: 6),
            Expanded(
              child: SizedBox(
                height: 8,
                child: Stack(
                  clipBehavior: Clip.none,
                  children: [
                    Positioned.fill(
                      child: Container(
                        clipBehavior: Clip.antiAlias,
                        decoration: BoxDecoration(
                          color: fill(context),
                          borderRadius: BorderRadius.circular(999),
                        ),
                        child: FractionallySizedBox(
                          alignment: Alignment.centerLeft,
                          widthFactor: frac,
                          child: Container(
                            color: red ? FemmeColors.bad : FemmeColors.accent,
                          ),
                        ),
                      ),
                    ),
                    for (var i = 1; i < XpBar.segments; i++)
                      Positioned.fill(
                        child: FractionallySizedBox(
                          alignment: Alignment.centerLeft,
                          widthFactor: i / XpBar.segments,
                          child: Align(
                            alignment: Alignment.centerRight,
                            child: Container(
                              key: ValueKey('xp-notch-$i'),
                              width: 1,
                              color: rule,
                            ),
                          ),
                        ),
                      ),
                    Positioned.fill(
                      child: Center(
                        child: OverflowBox(
                          maxWidth: 40,
                          maxHeight: 40,
                          child: AnimatedBuilder(
                            animation: _ring,
                            builder: (_, _) {
                              if (!_ring.isAnimating) {
                                return const SizedBox.shrink();
                              }
                              final t = Curves.easeOutQuint.transform(
                                _ring.value,
                              );
                              final d = 14 + 22 * t;
                              return Container(
                                key: const ValueKey('xp-ring'),
                                width: d,
                                height: d,
                                decoration: BoxDecoration(
                                  shape: BoxShape.circle,
                                  border: Border.all(
                                    color: FemmeColors.neon.withValues(
                                      alpha: 1 - t,
                                    ),
                                    width: 2,
                                  ),
                                ),
                              );
                            },
                          ),
                        ),
                      ),
                    ),
                  ],
                ),
              ),
            ),
            const SizedBox(width: 6),
            Text(
              c == null || c.window == 0 ? '–' : '${(frac * 100).round()}%',
              style: mono(
                context,
                size: 11,
                color: red ? FemmeColors.bad : muted(context),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// `◎ goal · <condition> · working | not met yet`, `✓ <condition> achieved`; hidden when cleared.
class GoalStrip extends StatelessWidget {
  const GoalStrip(this.goal, {super.key});
  final Goal? goal;

  @override
  Widget build(BuildContext context) {
    final g = goal;
    if (g == null || g.state == GoalState.cleared) {
      return const SizedBox.shrink();
    }
    final text = switch (g.state) {
      GoalState.met => '✓ ${g.condition} · achieved',
      GoalState.notMet => '◎ goal · ${g.condition} · not met yet',
      _ => '◎ goal · ${g.condition} · working',
    };
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: () => _note(
        context,
        'Goal',
        '${g.condition}${g.reason == null ? '' : '\n\n${g.reason}'}',
      ),
      child: Container(
        width: double.infinity,
        constraints: const BoxConstraints(minHeight: 32),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
        child: Text(
          text,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: mono(
            context,
            size: 12,
            color: g.state == GoalState.met ? FemmeColors.work : muted(context),
          ),
        ),
      ),
    );
  }
}

/// `✻ Thinking… (5m 42s · ↓ 15.0k tokens)`, redrawn every second while shown; the tokens are
/// counted from the main thread's items of the turn (desktop working.rs).
class WorkingLine extends StatefulWidget {
  const WorkingLine({super.key, required this.verb, this.since});
  final String verb;
  final int? since;

  @override
  State<WorkingLine> createState() => _WorkingLineState();
}

class _WorkingLineState extends State<WorkingLine> {
  Timer? _t;

  @override
  void initState() {
    super.initState();
    _t = Timer.periodic(const Duration(seconds: 1), (_) {
      if (mounted) setState(() {});
    });
  }

  @override
  void dispose() {
    _t?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final store = Femme.of(context).store;
    final main = [
      for (final i in store.items(ChatScope.of(context).sid))
        if (i.parent == null) i,
    ];
    final bytes = turnBytes(main, store.textOf);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 6, 16, 6),
      child: Text(
        workingText(
          widget.verb,
          widget.since,
          bytes,
          DateTime.now().millisecondsSinceEpoch,
        ),
        style: mono(context, size: 13, color: FemmeColors.neon),
      ),
    );
  }
}
