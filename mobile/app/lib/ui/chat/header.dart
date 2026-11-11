/// The chat's header: the title (tap = session info), the status / model / permission line
/// under it, and the `…` menu (terminal, files, info, rename, close).
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/models.dart' hide Row;
import '../../net/rpc.dart';
import '../../state/chat.dart';
import '../femme.dart';
import '../files.dart';
import '../session_info.dart';
import '../terminal.dart';
import '../widgets.dart';
import 'look.dart';
import 'turns.dart';

ObstructingPreferredSizeWidget chatNavBar(BuildContext context, String sid) {
  final s = Femme.of(context).store.sessions[sid];
  return CupertinoNavigationBar(
    middle: GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: () => showSessionInfo(context, sid),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Flexible(
            child: Text(
              s == null ? 'chat' : sessionTitle(s),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
          ),
          const SizedBox(width: 4),
          Icon(CupertinoIcons.info_circle, size: 16, color: muted(context)),
        ],
      ),
    ),
    trailing: CupertinoButton(
      padding: EdgeInsets.zero,
      minimumSize: const Size(44, 44),
      onPressed: () => showChatMenu(context, sid),
      child: const Icon(CupertinoIcons.ellipsis_circle),
    ),
  );
}

/// The line under the header: status dot and word, model · effort, permission mode.
class SubtitleStrip extends StatelessWidget {
  const SubtitleStrip({super.key, required this.sid});
  final String sid;

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final s = f.store.sessions[sid];
    if (s == null) {
      return Padding(
        padding: const EdgeInsets.all(8),
        child: Text(
          f.link.isOnline ? 'loading…' : linkLine(f.link.state.value),
          style: mono(context, size: 12, color: muted(context)),
        ),
      );
    }
    final claude = s.agent == Agent.claude;
    return Container(
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: panel(context), width: 1)),
      ),
      padding: const EdgeInsets.symmetric(horizontal: 12),
      child: Wrap(
        spacing: 8,
        crossAxisAlignment: WrapCrossAlignment.center,
        children: [
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 8),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                StatusDot(statusColor(s)),
                const SizedBox(width: 6),
                Text(
                  f.link.isOnline ? wireStatus(s) : 'offline',
                  style: mono(context, size: 12, color: muted(context)),
                ),
              ],
            ),
          ),
          if (s.agent != Agent.shell) Chip(label: modelEffort(s)),
          if (claude)
            Chip(
              label: permissionLabel(s.permission),
              color: permissionColor(s.permission),
              onTap: () async {
                final err = await cyclePermission(f.link, sid);
                if (err != null && context.mounted) {
                  toast(context, 'permission mode: $err');
                }
              },
            ),
        ],
      ),
    );
  }
}

Future<void> showChatMenu(BuildContext context, String sid) async {
  final f = Femme.of(context);
  final foreground = hasForegroundTask(f.store.items(sid), (i) => i.bodyJson);
  final pick = await showCupertinoModalPopup<String>(
    context: context,
    builder: (ctx) => CupertinoActionSheet(
      actions: [
        if (foreground)
          CupertinoActionSheetAction(
            onPressed: () => Navigator.pop(ctx, 'background'),
            child: const Text('Background all'),
          ),
        for (final (k, label) in const [
          ('terminal', 'Terminal'),
          ('files', 'Files'),
          ('info', 'Session info'),
          ('rename', 'Rename'),
        ])
          CupertinoActionSheetAction(
            onPressed: () => Navigator.pop(ctx, k),
            child: Text(label),
          ),
        CupertinoActionSheetAction(
          isDestructiveAction: true,
          onPressed: () => Navigator.pop(ctx, 'close'),
          child: const Text('Close chat'),
        ),
      ],
      cancelButton: CupertinoActionSheetAction(
        onPressed: () => Navigator.pop(ctx),
        child: const Text('Cancel'),
      ),
    ),
  );
  if (!context.mounted) return;
  final nav = Navigator.of(context);
  switch (pick) {
    case 'background':
      backgroundAll(context, sid);
    case 'terminal':
      nav.push(
        CupertinoPageRoute<void>(builder: (_) => TerminalScreen(sid: sid)),
      );
    case 'files':
      nav.push(CupertinoPageRoute<void>(builder: (_) => FilesScreen(sid: sid)));
    case 'info':
      showSessionInfo(context, sid);
    case 'rename':
      renameChat(context, sid);
    case 'close':
      closeChat(context, sid);
  }
}

/// Moves every foreground task of the chat to the background (`Background` with no call id);
/// the turn goes on.
Future<void> backgroundAll(BuildContext context, String sid) async {
  HapticFeedback.selectionClick();
  try {
    await Femme.of(context).link.req(Req.background(sid), resend: false);
  } catch (e) {
    if (context.mounted) {
      toast(context, 'not moved to the background: ${errorText(e)}');
    }
  }
}

Future<void> renameChat(BuildContext context, String sid) async {
  final f = Femme.of(context);
  final s = f.store.sessions[sid];
  final text = TextEditingController(text: s?.label ?? '');
  final ok = await showCupertinoDialog<bool>(
    context: context,
    builder: (ctx) => CupertinoAlertDialog(
      title: const Text('Rename chat'),
      content: Padding(
        padding: const EdgeInsets.only(top: 8),
        child: CupertinoTextField(
          controller: text,
          autofocus: true,
          placeholder: s == null ? 'label' : sessionTitle(s),
          onSubmitted: (_) => Navigator.pop(ctx, true),
        ),
      ),
      actions: [
        CupertinoDialogAction(
          onPressed: () => Navigator.pop(ctx, false),
          child: const Text('Cancel'),
        ),
        CupertinoDialogAction(
          isDefaultAction: true,
          onPressed: () => Navigator.pop(ctx, true),
          child: const Text('Save'),
        ),
      ],
    ),
  );
  final t = text.text.trim();
  text.dispose();
  if (ok != true) return;
  try {
    await f.link.req(
      Req.sessionLabel(sid, t.isEmpty ? null : t),
      resend: false,
    );
  } catch (e) {
    if (context.mounted) toast(context, 'rename: ${errorText(e)}');
  }
}

Future<void> closeChat(BuildContext context, String sid) async {
  final f = Femme.of(context);
  final ok = await showCupertinoDialog<bool>(
    context: context,
    builder: (ctx) => CupertinoAlertDialog(
      title: const Text('Close this chat?'),
      content: const Text('It stops the agent.'),
      actions: [
        CupertinoDialogAction(
          onPressed: () => Navigator.pop(ctx, false),
          child: const Text('Cancel'),
        ),
        CupertinoDialogAction(
          isDestructiveAction: true,
          onPressed: () => Navigator.pop(ctx, true),
          child: const Text('Close chat'),
        ),
      ],
    ),
  );
  if (ok != true) return;
  try {
    await f.link.req(Req.sessionClose(sid), resend: false);
    if (context.mounted) Navigator.of(context).maybePop();
  } catch (e) {
    if (context.mounted) toast(context, 'close: ${errorText(e)}');
  }
}
