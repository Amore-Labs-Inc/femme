/// The thin lines around the composer: the context XP bar, the goal strip, the working line.
library;

import 'dart:async';

import 'package:flutter/cupertino.dart';

import '../../net/models.dart' hide Row;
import '../../state/chat.dart';
import '../theme.dart';
import 'look.dart';

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

/// Fill = used / where the agent compacts (its window while it has not said; 0 while unknown), red
/// from 90 %, level = compactions + 1.
class XpBar extends StatelessWidget {
  const XpBar(this.context_, {super.key});
  final Context? context_;

  @override
  Widget build(BuildContext context) {
    final c = context_;
    final frac = c?.fraction ?? 0;
    final level = (c?.compactions ?? 0) + 1;
    final red = frac >= 0.9;
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: () => _note(
        context,
        'Context · level $level',
        c == null || c.full == 0
            ? 'The agent has not reported its context window yet.'
            : 'current context ${grouped(c.used)} / ${c.window == 0 ? '?' : grouped(c.window)}\n'
                  '${c.compactAt == null ? '' : 'compacts at ${grouped(c.compactAt!)}\n'}'
                  'used ${(frac * 100).toStringAsFixed(1)} %\n'
                  'room left ${grouped(c.full > c.used ? c.full - c.used : 0)}\n'
                  'compactions ${c.compactions}',
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
              child: Container(
                height: 6,
                decoration: BoxDecoration(
                  border: Border.all(color: ink(context), width: 1),
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

/// `✻ Thinking… (5m 42s)`, redrawn every second while shown.
class WorkingLine extends StatefulWidget {
  const WorkingLine({super.key, required this.verb, this.since});
  final String verb;
  final int? since;

  @override
  State<WorkingLine> createState() => _WorkingLineState();
}

class _WorkingLineState extends State<WorkingLine> {
  static const _spin = ['·', '✢', '✳', '✶', '✻', '✽'];
  Timer? _t;
  int _n = 0;

  @override
  void initState() {
    super.initState();
    _t = Timer.periodic(const Duration(milliseconds: 500), (_) {
      if (mounted) setState(() => _n++);
    });
  }

  @override
  void dispose() {
    _t?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final s = widget.since;
    final el = s == null
        ? ''
        : ' (${duration(DateTime.now().millisecondsSinceEpoch - s)})';
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 6, 16, 6),
      child: Text(
        '${_spin[_n % _spin.length]} ${widget.verb}…$el',
        style: mono(context, size: 13, color: FemmeColors.neon),
      ),
    );
  }
}
