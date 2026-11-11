import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../net/link.dart';
import '../net/models.dart' hide Row;
import '../net/rpc.dart';
import '../state/box.dart';
import 'chat.dart';
import 'femme.dart';
import 'files.dart';
import 'terminal.dart';
import 'theme.dart';

/// The desktop's lanes (`desktop/src/sidebar.rs` `Lane::of`): working, needs you (amber), idle
/// or starting (queue), failed (interrupted, paused, crashed, failed: red), closed.
Color statusColor(Session s) {
  if (s.status == SessionStatus.working) return FemmeColors.work;
  if (s.needsYou) return FemmeColors.pause;
  return switch (s.status) {
    SessionStatus.interrupted ||
    SessionStatus.paused ||
    SessionStatus.failed ||
    SessionStatus.crashed => FemmeColors.bad,
    SessionStatus.closed => FemmeColors.done,
    _ => FemmeColors.queue,
  };
}

/// The status dot: an 8 px ink-bordered square in the lane colour.
class StatusDot extends StatelessWidget {
  const StatusDot(this.color, {super.key});
  final Color color;

  @override
  Widget build(BuildContext context) => Container(
    width: 8,
    height: 8,
    decoration: BoxDecoration(
      color: color,
      border: Border.all(
        color: FemmeColors.inkC.resolveFrom(context),
        width: 1.2,
      ),
    ),
  );
}

class SessionTile extends StatelessWidget {
  const SessionTile(this.session, {super.key, this.subtitle, this.onTap});
  final Session session;
  final String? subtitle;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) => CupertinoListTile(
    leading: StatusDot(statusColor(session)),
    leadingSize: 8,
    title: Text(session.title),
    subtitle: Text(
      subtitle ?? '${wireStatus(session)} · ${session.cwd}',
      maxLines: 1,
      overflow: TextOverflow.ellipsis,
    ),
    onTap: onTap,
  );
}

String wireStatus(Session s) => s.needsYou ? 'needs you' : s.status.name;

/// `ws 41 ms · femmed 0.9.1` (`online · …` before the first round-trip sample) / `connecting…` /
/// `offline: <why>`.
String linkLine(LinkState s) => switch (s.phase) {
  LinkPhase.online =>
    '${s.rtt == null ? 'online' : 'ws ${rttText(s.rtt!)}'}'
        '${s.serverVersion == null ? '' : ' · femmed ${s.serverVersion}'}',
  LinkPhase.connecting => 'connecting…',
  LinkPhase.offline =>
    'offline${s.reason == null ? '' : ': ${s.reason}'}${_retry(s.retryAt)}',
};

/// A round-trip time in whole milliseconds: `41 ms`.
String rttText(Duration d) => '${(d.inMicroseconds / 1000).round()} ms';

String _retry(DateTime? at) {
  if (at == null) return '';
  final s = at.difference(DateTime.now()).inSeconds;
  return s > 0 ? ' · retry in $s s' : '';
}

Color linkColor(LinkState s) => switch (s.phase) {
  LinkPhase.online => FemmeColors.work,
  LinkPhase.connecting => FemmeColors.pause,
  LinkPhase.offline => FemmeColors.bad,
};

/// A tab's page: large title, the link line under it, then [slivers].
class TabPage extends StatelessWidget {
  const TabPage({
    super.key,
    required this.title,
    required this.status,
    required this.slivers,
    this.leading,
    this.trailing,
    this.onRefresh,
    this.dot,
  });
  final String title;
  final String status;
  final List<Widget> slivers;
  final Widget? leading;
  final Widget? trailing;
  final Future<void> Function()? onRefresh;

  /// A coloured dot before [status] (the link's phase).
  final Color? dot;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    child: CustomScrollView(
      physics: const AlwaysScrollableScrollPhysics(),
      slivers: [
        CupertinoSliverNavigationBar(
          largeTitle: Text(title),
          leading: leading,
          trailing: trailing,
        ),
        if (onRefresh != null)
          CupertinoSliverRefreshControl(onRefresh: onRefresh),
        SliverToBoxAdapter(
          child: Padding(
            padding: const EdgeInsets.fromLTRB(20, 4, 20, 4),
            child: Row(
              children: [
                if (dot != null) ...[StatusDot(dot!), const SizedBox(width: 8)],
                Expanded(
                  child: Text(
                    status,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      fontSize: 13,
                      color: FemmeColors.mutedC.resolveFrom(context),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ),
        ...slivers,
        const SliverPadding(padding: EdgeInsets.only(bottom: 24)),
      ],
    ),
  );
}

/// A titled list section, or nothing when [children] is empty.
Widget section(String header, List<Widget> children, {Widget? footer}) =>
    SliverToBoxAdapter(
      child: children.isEmpty
          ? const SizedBox.shrink()
          : CupertinoListSection.insetGrouped(
              header: Text(header),
              footer: footer,
              children: children,
            ),
    );

/// A grey centred line for empty states.
Widget emptyLine(String text) => SliverFillRemaining(
  hasScrollBody: false,
  child: Center(
    child: Text(text, style: const TextStyle(color: FemmeColors.done)),
  ),
);

// ---- small helpers ----

/// A shell, or an agent run in its TUI: its centre is the terminal, not the chat
/// (`desktop/src/app.rs` shows a `TerminalView` for these).
bool opensTerminal(Session? s) =>
    s != null && (s.agent == Agent.shell || s.mode == Mode.tui);

/// What tapping a session opens: [TerminalScreen] for a shell or TUI session, else [ChatScreen]
/// (also while the store has not seen the session yet).
Widget sessionScreen(Session? s, String sid) =>
    opensTerminal(s) ? TerminalScreen(sid: sid) : ChatScreen(sid: sid);

/// Opens a session on the current tab's navigator: the terminal for a shell or TUI session, the
/// chat otherwise.
Future<void> pushChat(BuildContext context, String sid) {
  final s = Femme.of(context).store.session(sid);
  return Navigator.of(context)
      .push(CupertinoPageRoute<void>(builder: (_) => sessionScreen(s, sid)));
}

void tapHaptic() => HapticFeedback.selectionClick();
void actionHaptic() => HapticFeedback.mediumImpact();

/// `now`, `5m`, `3h`, `2d` ago from [now].
String ago(DateTime t, {DateTime? now}) {
  final d = (now ?? DateTime.now()).difference(t);
  if (d.inMinutes < 1) return 'now';
  if (d.inHours < 1) return '${d.inMinutes}m';
  if (d.inDays < 1) return '${d.inHours}h';
  return '${d.inDays}d';
}

/// `4:05`, `1:02:03`: an elapsed time.
String elapsed(Duration d) {
  final s = d.inSeconds.clamp(0, 1 << 31);
  final h = s ~/ 3600, m = (s % 3600) ~/ 60, sec = s % 60;
  final mm = h > 0 ? m.toString().padLeft(2, '0') : '$m';
  return '${h > 0 ? '$h:' : ''}$mm:${sec.toString().padLeft(2, '0')}';
}

/// `/home/<u>`, `/Users/<u>` and `/root` as `~`.
String homeShort(String path) {
  final p = path.endsWith('/') && path.length > 1
      ? path.substring(0, path.length - 1)
      : path;
  final m = RegExp(r'^(/home/[^/]+|/Users/[^/]+|/root)(/.*)?$').firstMatch(p);
  if (m == null) return p;
  return '~${m.group(2) ?? ''}';
}

/// Asks before a destructive or irreversible action. True when confirmed.
Future<bool> confirmAction(
  BuildContext context, {
  required String title,
  String? message,
  required String action,
  bool destructive = true,
}) async {
  final ok = await showCupertinoDialog<bool>(
    context: context,
    builder: (c) => CupertinoAlertDialog(
      title: Text(title),
      content: message == null ? null : Text(message),
      actions: [
        CupertinoDialogAction(
          isDefaultAction: true,
          onPressed: () => Navigator.pop(c, false),
          child: const Text('Cancel'),
        ),
        CupertinoDialogAction(
          isDestructiveAction: destructive,
          onPressed: () => Navigator.pop(c, true),
          child: Text(action),
        ),
      ],
    ),
  );
  return ok ?? false;
}

/// Shows what went wrong with an action (`box too old for this` for `Unsupported`).
Future<void> showProblem(BuildContext context, String what, Object e) {
  HapticFeedback.heavyImpact();
  return showCupertinoDialog<void>(
    context: context,
    builder: (c) => CupertinoAlertDialog(
      title: Text(what),
      content: Text(describeError(e)),
      actions: [
        CupertinoDialogAction(
          isDefaultAction: true,
          onPressed: () => Navigator.pop(c),
          child: const Text('OK'),
        ),
      ],
    ),
  );
}

/// A sheet page with Cancel leading and [doneLabel] trailing.
class SheetPage extends StatelessWidget {
  const SheetPage({
    super.key,
    required this.title,
    required this.child,
    this.onDone,
    this.doneLabel = 'Done',
    this.busy = false,
  });
  final String title;
  final Widget child;

  /// Null greys Done out.
  final VoidCallback? onDone;
  final String doneLabel;
  final bool busy;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    backgroundColor: CupertinoColors.systemGroupedBackground,
    navigationBar: CupertinoNavigationBar(
      automaticallyImplyLeading: false,
      middle: Text(title),
      leading: CupertinoButton(
        padding: EdgeInsets.zero,
        onPressed: () => Navigator.of(context).pop(),
        child: const Text('Cancel'),
      ),
      trailing: busy
          ? const CupertinoActivityIndicator()
          : CupertinoButton(
              padding: EdgeInsets.zero,
              onPressed: onDone,
              child: Text(
                doneLabel,
                style: const TextStyle(fontWeight: FontWeight.w600),
              ),
            ),
    ),
    child: SafeArea(child: child),
  );
}

/// One line of text in a sheet; null when cancelled.
Future<String?> textSheet(
  BuildContext context, {
  required String title,
  String initial = '',
  String placeholder = '',
}) {
  final ctl = TextEditingController(text: initial);
  return showCupertinoSheet<String>(
    context: context,
    scrollableBuilder: (c, _) => SheetPage(
      title: title,
      onDone: () => Navigator.of(c).pop(ctl.text),
      child: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          CupertinoTextField(
            controller: ctl,
            autofocus: true,
            placeholder: placeholder,
            clearButtonMode: OverlayVisibilityMode.editing,
            onSubmitted: (v) => Navigator.of(c).pop(v),
          ),
        ],
      ),
    ),
  );
}

// ---- session actions ----

Future<void> renameSession(BuildContext context, Session s) async {
  final link = Femme.of(context).link;
  final v = await textSheet(
    context,
    title: 'Rename',
    initial: s.label ?? s.title,
    placeholder: 'label (empty clears it)',
  );
  if (v == null) return;
  final label = v.trim().isEmpty ? null : v.trim();
  try {
    await link.req(Req.sessionLabel(s.id, label));
    actionHaptic();
  } catch (e) {
    if (context.mounted) await showProblem(context, 'Rename failed', e);
  }
}

/// Asks, then ends the session for real (holder and agent stop). True when closed.
Future<bool> closeSession(BuildContext context, Session s) async {
  final link = Femme.of(context).link;
  final ok = await confirmAction(
    context,
    title: 'Close ${s.title}?',
    message: 'The agent stops and the session ends.',
    action: 'Close',
  );
  if (!ok) return false;
  try {
    await link.req(Req.sessionClose(s.id));
    actionHaptic();
    return true;
  } catch (e) {
    if (context.mounted) await showProblem(context, 'Close failed', e);
    return false;
  }
}

/// A session row: tap opens the chat (the terminal for a shell / TUI session), swipe left closes
/// (confirmed), long-press opens a menu (Rename, Terminal or Chat, Files, Close).
class SessionActions extends StatelessWidget {
  const SessionActions({super.key, required this.session, required this.child});
  final Session session;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final s = session;
    final nav = Navigator.of(context);
    void go(Widget page) {
      Navigator.of(context, rootNavigator: true).pop();
      nav.push(CupertinoPageRoute<void>(builder: (_) => page));
    }

    return Dismissible(
      key: ValueKey('row-${s.id}'),
      direction: DismissDirection.endToStart,
      confirmDismiss: (_) => closeSession(context, s),
      background: Container(
        color: CupertinoColors.destructiveRed,
        alignment: Alignment.centerRight,
        padding: const EdgeInsets.symmetric(horizontal: 20),
        child: const Text(
          'Close',
          style: TextStyle(
            color: CupertinoColors.white,
            fontWeight: FontWeight.w600,
          ),
        ),
      ),
      child: CupertinoContextMenu(
        enableHapticFeedback: true,
        actions: [
          CupertinoContextMenuAction(
            trailingIcon: CupertinoIcons.pencil,
            onPressed: () {
              Navigator.of(context, rootNavigator: true).pop();
              renameSession(context, s);
            },
            child: const Text('Rename'),
          ),
          // The tap opens one of chat / terminal; the menu reaches the other.
          if (opensTerminal(s))
            CupertinoContextMenuAction(
              trailingIcon: CupertinoIcons.bubble_left,
              onPressed: () => go(ChatScreen(sid: s.id)),
              child: const Text('Chat'),
            )
          else
            CupertinoContextMenuAction(
              trailingIcon: CupertinoIcons.chevron_left_slash_chevron_right,
              onPressed: () => go(TerminalScreen(sid: s.id)),
              child: const Text('Terminal'),
            ),
          CupertinoContextMenuAction(
            trailingIcon: CupertinoIcons.folder,
            onPressed: () => go(FilesScreen(sid: s.id)),
            child: const Text('Files'),
          ),
          CupertinoContextMenuAction(
            isDestructiveAction: true,
            trailingIcon: CupertinoIcons.xmark_circle,
            onPressed: () {
              Navigator.of(context, rootNavigator: true).pop();
              closeSession(context, s);
            },
            child: const Text('Close'),
          ),
        ],
        child: ColoredBox(
          color: CupertinoColors.secondarySystemGroupedBackground.resolveFrom(
            context,
          ),
          child: child,
        ),
      ),
    );
  }
}
