/// A subagent's own transcript (the items whose `parent` is its `Agent` call id), and the strip
/// of chips above the transcript that leads to them: `‹ main`, then one chip per subagent of the
/// turn with its elapsed time (desktop live.rs `strip`, agents.rs).
library;

import 'dart:async';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../state/chat.dart';
import '../theme.dart';
import 'look.dart';
import 'media.dart';
import 'transcript.dart';
import 'turns.dart';

class SubagentScreen extends StatelessWidget {
  const SubagentScreen({
    super.key,
    required this.sid,
    required this.callId,
    required this.title,
    required this.controller,
  });
  final String sid;
  final String callId;
  final String title;

  /// The chat's controller (the chat screen below owns it).
  final ChatController controller;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    navigationBar: CupertinoNavigationBar(
      middle: Text(title, maxLines: 1, overflow: TextOverflow.ellipsis),
      previousPageTitle: 'main',
    ),
    child: SafeArea(
      child: ChatScope(
        controller: controller,
        child: TranscriptView(
          sid: sid,
          parent: callId,
          empty: 'The subagent has not said anything yet.',
        ),
      ),
    ),
  );
}

/// Opens the subagent of the call [callId]; from a subagent view the new one takes its place.
void openSubagent(
  BuildContext context,
  ChatController c,
  String callId,
  String title, {
  bool replace = false,
}) {
  final route = CupertinoPageRoute<void>(
    builder: (_) =>
        SubagentScreen(sid: c.sid, callId: callId, title: title, controller: c),
  );
  final nav = Navigator.of(context);
  replace ? nav.pushReplacement(route) : nav.push(route);
}

/// `‹ main` and one chip per subagent; the thread on show ([view], null = main) is lit. The
/// running ones count their time every second.
class AgentStrip extends StatefulWidget {
  const AgentStrip({super.key, required this.chips, this.view});
  final List<AgentChip> chips;
  final String? view;

  @override
  State<AgentStrip> createState() => _AgentStripState();
}

class _AgentStripState extends State<AgentStrip> {
  Timer? _tick;

  @override
  void initState() {
    super.initState();
    _arm();
  }

  @override
  void didUpdateWidget(AgentStrip old) {
    super.didUpdateWidget(old);
    _arm();
  }

  void _arm() {
    final live = widget.chips.any((c) => c.running);
    if (live && _tick == null) {
      _tick = Timer.periodic(const Duration(seconds: 1), (_) {
        if (mounted) setState(() {});
      });
    } else if (!live) {
      _tick?.cancel();
      _tick = null;
    }
  }

  @override
  void dispose() {
    _tick?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final now = DateTime.now().millisecondsSinceEpoch;
    final inSub = widget.view != null;
    return Container(
      height: 44,
      decoration: BoxDecoration(
        color: paper(context),
        border: Border(
          bottom: BorderSide(color: ink(context).withValues(alpha: 0.16)),
        ),
      ),
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 8),
        children: [
          _Tab(
            on: !inSub,
            onTap: inSub ? () => Navigator.of(context).maybePop() : null,
            children: [
              Text(
                '‹ main',
                style: mono(
                  context,
                  size: 12,
                  color: !inSub ? ink(context) : muted(context),
                ),
              ),
            ],
          ),
          for (final a in widget.chips)
            _Tab(
              on: widget.view == a.id,
              onTap: widget.view == a.id
                  ? null
                  : () {
                      HapticFeedback.selectionClick();
                      openSubagent(context, c, a.id, a.label, replace: inSub);
                    },
              children: [
                Container(
                  width: 7,
                  height: 7,
                  decoration: BoxDecoration(
                    shape: BoxShape.circle,
                    color: a.running ? FemmeColors.neon : FemmeColors.done,
                  ),
                ),
                const SizedBox(width: 5),
                ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 180),
                  child: Text(
                    a.label,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: mono(
                      context,
                      size: 12,
                      color: a.running ? ink(context) : muted(context),
                    ),
                  ),
                ),
                const SizedBox(width: 5),
                Text(
                  a.running ? agentElapsed(a.since, now) : 'done',
                  style: mono(context, size: 12, color: muted(context)),
                ),
              ],
            ),
        ],
      ),
    );
  }
}

class _Tab extends StatelessWidget {
  const _Tab({required this.on, required this.children, this.onTap});
  final bool on;
  final VoidCallback? onTap;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) => GestureDetector(
    behavior: HitTestBehavior.opaque,
    onTap: onTap,
    child: Container(
      margin: const EdgeInsets.symmetric(vertical: 6, horizontal: 2),
      padding: const EdgeInsets.symmetric(horizontal: 8),
      color: on ? panel(context) : null,
      child: Row(mainAxisSize: MainAxisSize.min, children: children),
    ),
  );
}
