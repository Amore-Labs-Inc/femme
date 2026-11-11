/// The session sheet: everything the box knows about one chat (name, agent, goal, context,
/// where it runs), with the label, the permission mode and closing the chat as actions.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:intl/intl.dart';

import '../net/models.dart' hide Row;
import '../net/rpc.dart';
import '../state/chat.dart';
import 'chat/look.dart';
import 'femme.dart';
import 'files.dart';
import 'terminal.dart';
import 'theme.dart';

/// Shows the sheet for [sid]; it follows the session live until closed.
Future<void> showSessionInfo(BuildContext context, String sid) {
  return showCupertinoModalPopup<void>(
    context: context,
    builder: (_) => _SessionSheet(sid: sid, outer: context),
  );
}

class _SessionSheet extends StatelessWidget {
  const _SessionSheet({required this.sid, required this.outer});
  final String sid;

  /// The context that opened the sheet (the chat screen): pages are pushed from it, and it is
  /// popped after the chat is closed.
  final BuildContext outer;

  @override
  Widget build(BuildContext context) {
    final mq = MediaQuery.of(context);
    final s = Femme.of(context).store.sessions[sid];
    final bg = panel(context);
    return MediaQuery.removePadding(
      context: context,
      removeTop: true,
      child: Container(
        height: mq.size.height - mq.padding.top - 12,
        decoration: BoxDecoration(
          color: bg,
          border: Border(top: BorderSide(color: ink(context), width: 1.2)),
        ),
        child: CupertinoPageScaffold(
          backgroundColor: bg,
          navigationBar: CupertinoNavigationBar(
            automaticallyImplyLeading: false,
            transitionBetweenRoutes: false,
            backgroundColor: bg,
            middle: const Text('Session'),
            trailing: CupertinoButton(
              padding: EdgeInsets.zero,
              minimumSize: const Size(44, 44),
              onPressed: () => Navigator.of(context).pop(),
              child: const Text(
                'Done',
                style: TextStyle(fontWeight: FontWeight.w600),
              ),
            ),
          ),
          child: SafeArea(
            top: false,
            child: s == null
                ? Center(
                    child: Text(
                      'this chat is gone',
                      style: TextStyle(color: muted(context), fontSize: 16),
                    ),
                  )
                : _Body(session: s, outer: outer),
          ),
        ),
      ),
    );
  }
}

class _Body extends StatelessWidget {
  const _Body({required this.session, required this.outer});
  final Session session;
  final BuildContext outer;

  String get sid => session.id;

  @override
  Widget build(BuildContext context) {
    final s = session;
    final femme = Femme.of(context);
    final g = s.goal;
    final ctx = s.context;
    final date = DateFormat('d MMM y, HH:mm');
    String when(int ms) =>
        date.format(DateTime.fromMillisecondsSinceEpoch(ms).toLocal());

    return ListView(
      padding: const EdgeInsets.only(bottom: 24),
      children: [
        _section(
          context,
          header: const _Header('Name'),
          children: [
            _Row('title', sessionTitle(s)),
            _Row(
              'label',
              s.label == null || s.label!.isEmpty ? 'none' : s.label!,
              dim: s.label == null || s.label!.isEmpty,
              copy: s.label,
              onTap: () => _editLabel(context),
              chevron: true,
            ),
          ],
        ),
        _section(
          context,
          header: const _Header('Agent'),
          children: [
            _Row('agent', s.agent.name),
            _Row('mode', s.mode.name),
            _Row(
              'model',
              s.model == null || s.model!.isEmpty
                  ? 'unknown'
                  : modelLabel(s.model),
              sub: s.model == null || s.model!.isEmpty ? null : s.model,
              copy: s.model,
              dim: s.model == null || s.model!.isEmpty,
            ),
            _Row(
              'effort',
              s.effort == null || s.effort!.isEmpty ? 'default' : s.effort!,
              dim: s.effort == null || s.effort!.isEmpty,
            ),
            _Row(
              'permission',
              permissionLabel(s.permission),
              lead: _Square(permissionColor(s.permission)),
              onTap: () async {
                final err = await cyclePermission(femme.link, sid);
                if (err != null && context.mounted) toast(context, err);
              },
            ),
            _Row(
              'status',
              s.attention == Attention.none
                  ? s.status.name
                  : '${s.status.name} · ${s.attention.name}',
            ),
          ],
        ),
        if (g != null)
          _section(
            context,
            header: const _Header('Goal'),
            children: [
              _Row('condition', g.condition),
              _Row('state', _goalState(g.state)),
              if (g.reason != null && g.reason!.isNotEmpty)
                _Row('reason', g.reason!),
            ],
          ),
        _section(
          context,
          header: const _Header('Context'),
          children: [
            if (ctx == null)
              const _Row('used', 'not reported yet', dim: true)
            else if (ctx.window == 0)
              _Row('used', '${grouped(ctx.used)} · window unknown')
            else
              _Row(
                'used',
                '${grouped(ctx.used)} / ${grouped(ctx.window)} · '
                    '${(ctx.used * 100 / ctx.window).round()}%',
                color: ctx.used / ctx.window > 0.9 ? FemmeColors.bad : null,
              ),
            if (ctx != null) ...[
              if (ctx.compactAt != null)
                _Row('compacts at', grouped(ctx.compactAt!)),
              _Row('compactions', '${ctx.compactions}'),
              _Row('level', '${ctx.compactions + 1}'),
            ],
          ],
        ),
        _section(
          context,
          header: const _Header('Where'),
          children: [
            _Row('cwd', s.cwd, monospace: true),
            _Row('handle', s.handle, monospace: true),
            _Row('id', s.id, monospace: true),
            if (s.agentSid != null && s.agentSid!.isNotEmpty)
              _Row('agent session', s.agentSid!, monospace: true),
            _Row('created', when(s.created)),
            if (s.closed != null) _Row('closed', when(s.closed!)),
          ],
        ),
        _section(
          context,
          header: const _Header('Actions'),
          children: [
            _Action(
              icon: CupertinoIcons.chevron_left_slash_chevron_right,
              label: 'Terminal',
              onTap: () => Navigator.of(outer).push(
                CupertinoPageRoute<void>(
                  builder: (_) => TerminalScreen(sid: sid),
                ),
              ),
            ),
            _Action(
              icon: CupertinoIcons.folder,
              label: 'Files',
              onTap: () => Navigator.of(outer).push(
                CupertinoPageRoute<void>(builder: (_) => FilesScreen(sid: sid)),
              ),
            ),
            if (s.status != SessionStatus.closed)
              _Action(
                icon: CupertinoIcons.xmark_circle,
                label: 'Close chat',
                color: FemmeColors.bad,
                onTap: () => _close(context),
              ),
          ],
        ),
      ],
    );
  }

  static String _goalState(GoalState g) => switch (g) {
    GoalState.active => 'working',
    GoalState.notMet => 'not met yet',
    GoalState.met => 'achieved',
    GoalState.cleared => 'cleared',
  };

  Future<void> _editLabel(BuildContext context) async {
    final link = Femme.of(context).link;
    final text = TextEditingController(text: session.label ?? '');
    final ok = await showCupertinoDialog<bool>(
      context: context,
      builder: (d) => CupertinoAlertDialog(
        title: const Text('Label'),
        content: Padding(
          padding: const EdgeInsets.only(top: 12),
          child: CupertinoTextField(
            controller: text,
            autofocus: true,
            placeholder: 'no label',
            clearButtonMode: OverlayVisibilityMode.editing,
            onSubmitted: (_) => Navigator.of(d).pop(true),
          ),
        ),
        actions: [
          CupertinoDialogAction(
            onPressed: () => Navigator.of(d).pop(false),
            child: const Text('Cancel'),
          ),
          CupertinoDialogAction(
            isDefaultAction: true,
            onPressed: () => Navigator.of(d).pop(true),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    final value = text.text.trim();
    text.dispose();
    if (ok != true) return;
    try {
      await link.req(
        Req.sessionLabel(sid, value.isEmpty ? null : value),
        resend: false,
      );
    } catch (e) {
      if (context.mounted) toast(context, errorText(e));
    }
  }

  Future<void> _close(BuildContext context) async {
    final link = Femme.of(context).link;
    final ok = await showCupertinoDialog<bool>(
      context: context,
      builder: (d) => CupertinoAlertDialog(
        title: const Text('Close this chat?'),
        content: const Text('It stops the agent.'),
        actions: [
          CupertinoDialogAction(
            onPressed: () => Navigator.of(d).pop(false),
            child: const Text('Cancel'),
          ),
          CupertinoDialogAction(
            isDestructiveAction: true,
            onPressed: () => Navigator.of(d).pop(true),
            child: const Text('Close chat'),
          ),
        ],
      ),
    );
    if (ok != true) return;
    try {
      await link.req(Req.sessionClose(sid), resend: false);
    } catch (e) {
      if (context.mounted) toast(context, errorText(e));
      return;
    }
    if (context.mounted) Navigator.of(context).pop();
    if (outer.mounted) Navigator.of(outer).maybePop();
  }
}

Widget _section(
  BuildContext context, {
  required Widget header,
  required List<Widget> children,
}) => CupertinoListSection.insetGrouped(
  header: header,
  backgroundColor: panel(context),
  decoration: BoxDecoration(
    color: paper(context),
    border: Border.all(color: ink(context), width: 1.2),
  ),
  separatorColor: panel(context),
  children: children,
);

class _Header extends StatelessWidget {
  const _Header(this.text);
  final String text;

  @override
  Widget build(BuildContext context) => Text(
    text.toUpperCase(),
    style: TextStyle(color: muted(context), fontSize: 13),
  );
}

class _Square extends StatelessWidget {
  const _Square(this.color);
  final Color color;

  @override
  Widget build(BuildContext context) => Container(
    width: 12,
    height: 12,
    decoration: BoxDecoration(
      color: color,
      border: Border.all(color: ink(context), width: 1.2),
    ),
  );
}

/// One label/value row: the value wraps, a long press copies it ([copy] or the value).
class _Row extends StatelessWidget {
  const _Row(
    this.label,
    this.value, {
    this.sub,
    this.copy,
    this.monospace = false,
    this.dim = false,
    this.color,
    this.lead,
    this.onTap,
    this.chevron = false,
  });
  final String label;
  final String value;
  final String? sub;
  final String? copy;
  final bool monospace;
  final bool dim;
  final Color? color;
  final Widget? lead;
  final VoidCallback? onTap;
  final bool chevron;

  @override
  Widget build(BuildContext context) {
    final c = color ?? (dim ? muted(context) : ink(context));
    final vStyle = monospace
        ? _mono(context, color: c)
        : TextStyle(color: c, fontSize: 16, height: 1.3);
    final toCopy = copy ?? (dim ? null : value);
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onTap,
      onLongPress: toCopy == null
          ? null
          : () {
              HapticFeedback.selectionClick();
              Clipboard.setData(ClipboardData(text: toCopy));
              toast(context, 'copied');
            },
      child: ConstrainedBox(
        constraints: const BoxConstraints(minHeight: 44),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 11),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SizedBox(
                width: 112,
                child: Text(
                  label,
                  style: TextStyle(
                    color: muted(context),
                    fontSize: 16,
                    height: 1.3,
                  ),
                ),
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    Row(
                      mainAxisAlignment: MainAxisAlignment.end,
                      crossAxisAlignment: CrossAxisAlignment.center,
                      children: [
                        if (lead != null) ...[lead!, const SizedBox(width: 8)],
                        Flexible(
                          child: Text(
                            value,
                            textAlign: TextAlign.end,
                            softWrap: true,
                            style: vStyle,
                          ),
                        ),
                      ],
                    ),
                    if (sub != null)
                      Padding(
                        padding: const EdgeInsets.only(top: 2),
                        child: Text(
                          sub!,
                          textAlign: TextAlign.end,
                          style: _mono(context, color: muted(context)),
                        ),
                      ),
                  ],
                ),
              ),
              if (chevron) ...[
                const SizedBox(width: 6),
                Padding(
                  padding: const EdgeInsets.only(top: 3),
                  child: Icon(
                    CupertinoIcons.chevron_forward,
                    size: 16,
                    color: muted(context),
                  ),
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }

  static TextStyle _mono(BuildContext context, {Color? color}) =>
      mono(context, size: 13, color: color);
}

class _Action extends StatelessWidget {
  const _Action({
    required this.icon,
    required this.label,
    required this.onTap,
    this.color,
  });
  final IconData icon;
  final String label;
  final VoidCallback onTap;
  final Color? color;

  @override
  Widget build(BuildContext context) {
    final c = color ?? ink(context);
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onTap,
      child: ConstrainedBox(
        constraints: const BoxConstraints(minHeight: 48),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
          child: Row(
            children: [
              Icon(icon, size: 20, color: c),
              const SizedBox(width: 12),
              Expanded(
                child: Text(label, style: TextStyle(color: c, fontSize: 16)),
              ),
              if (color == null)
                Icon(
                  CupertinoIcons.chevron_forward,
                  size: 16,
                  color: muted(context),
                ),
            ],
          ),
        ),
      ),
    );
  }
}
