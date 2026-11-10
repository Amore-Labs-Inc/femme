/// One widget per transcript row (BODIES.md §3): user bubble, peer card, assistant markdown,
/// thinking, plan, event notes, read marks, compaction and turn-end lines, folds.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:intl/intl.dart';

import '../../net/models.dart' hide Row;
import '../../net/rpc.dart';
import '../../state/chat.dart';
import '../../state/outbox.dart';
import '../femme.dart';
import '../theme.dart';
import 'attached.dart';
import 'look.dart';
import 'markdown.dart';
import 'media.dart';
import 'question.dart';
import 'tool_row.dart';
import 'turns.dart';

const _side = 16.0;

/// The widget for [e]. [all] are the session's items (for plan approvals and permission calls).
Widget entryRow(Entry e, List<Item> all) => switch (e) {
  ItemEntry(:final item) => switch (item.kind) {
    ItemKind.userMessage => UserBubble(item),
    ItemKind.peerMessage => PeerCard(item),
    ItemKind.assistantMessage => AssistantRow(item),
    ItemKind.thinking => ThinkingRow(item, ms: e.ms),
    ItemKind.plan => PlanRow(item, all),
    ItemKind.question => QuestionRow(item, all),
    ItemKind.event => EventRow(item),
    _ => const SizedBox.shrink(),
  },
  ToolEntry() => ToolRow(e),
  ReadEntry() => ReadRow(e),
  CompactionEntry() => CompactionRow(e.item),
  TurnEndEntry() => NoteLine(e.text, red: e.red),
  FoldEntry() => FoldRow(e, all),
};

String agentName(Agent a) => switch (a) {
  Agent.claude => 'Claude',
  Agent.codex => 'Codex',
  Agent.pi => 'pi',
  Agent.shell => 'The shell',
};

/// Long-press menu of a message: copy, and resend for one of ours.
Future<void> messageMenu(
  BuildContext context,
  String text, {
  VoidCallback? resend,
  VoidCallback? discard,
}) => showCupertinoModalPopup<void>(
  context: context,
  builder: (ctx) => CupertinoActionSheet(
    actions: [
      CupertinoActionSheetAction(
        onPressed: () {
          Clipboard.setData(ClipboardData(text: text));
          Navigator.pop(ctx);
          HapticFeedback.selectionClick();
        },
        child: const Text('Copy text'),
      ),
      if (resend != null)
        CupertinoActionSheetAction(
          onPressed: () {
            Navigator.pop(ctx);
            resend();
          },
          child: const Text('Resend'),
        ),
      if (discard != null)
        CupertinoActionSheetAction(
          isDestructiveAction: true,
          onPressed: () {
            Navigator.pop(ctx);
            discard();
          },
          child: const Text('Discard'),
        ),
    ],
    cancelButton: CupertinoActionSheetAction(
      onPressed: () => Navigator.pop(ctx),
      child: const Text('Cancel'),
    ),
  ),
);

/// The streaming cursor after text still being written.
class Cursor extends StatelessWidget {
  const Cursor({super.key});
  @override
  Widget build(BuildContext context) => Container(
    margin: const EdgeInsets.only(top: 2),
    width: 8,
    height: 14,
    color: FemmeColors.neon,
  );
}

class UserBubble extends StatelessWidget {
  const UserBubble(this.item, {super.key});
  final Item item;

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final c = ChatScope.of(context);
    final local = item.itemId.startsWith('local:');
    final raw = c.text(item, f.store.textOf);
    final split = splitAttached(raw);
    final op = local ? item.itemId.substring(6) : null;
    final entry = op == null
        ? null
        : f.store.outbox.where((o) => o.op == op).firstOrNull;
    final agent = f.store.sessions[item.sid]?.agent ?? Agent.claude;

    void resend() async {
      HapticFeedback.lightImpact();
      if (op != null) {
        f.store.retry(op).ignore();
      } else {
        final err = await c.run(Req.resend(item.sid, item.itemId));
        if (err != null && context.mounted) toast(context, 'resend: $err');
      }
    }

    final (String? line, bool red) = switch (item.status) {
      _ when entry != null => switch (entry.state) {
        OutboxState.sending => ('sending…', false),
        OutboxState.accepted => ('sent', false),
        OutboxState.failed => ('failed · retry', true),
      },
      ItemStatus.pending => ('queued', false),
      ItemStatus.failed => (
        "${agentName(agent)} didn't get this · resend",
        true,
      ),
      ItemStatus.completed when item.readAt != null => (
        'read ${DateFormat.Hm().format(DateTime.fromMillisecondsSinceEpoch(item.readAt!))}',
        false,
      ),
      _ => (null, false),
    };
    final faded = local && entry?.state != OutboxState.failed;
    final files = [
      ...split.paths,
      if (local && entry != null)
        for (final a in entry.attachments) a.name,
    ];

    return Padding(
      padding: const EdgeInsets.fromLTRB(48, 6, _side, 6),
      child: Opacity(
        opacity: faded ? 0.6 : 1,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.end,
          children: [
            if (split.text.isNotEmpty)
              GestureDetector(
                onLongPress: () => messageMenu(
                  context,
                  split.text,
                  resend: local ? null : resend,
                  discard: op == null ? null : () => f.store.discard(op),
                ),
                child: Container(
                  padding: const EdgeInsets.symmetric(
                    horizontal: 12,
                    vertical: 9,
                  ),
                  decoration: BoxDecoration(
                    color: FemmeColors.accent,
                    border: Border.all(color: ink(context), width: 1.2),
                    boxShadow: [
                      BoxShadow(
                        color: ink(context),
                        offset: const Offset(2, 2),
                      ),
                    ],
                  ),
                  child: Text(
                    split.text,
                    style: const TextStyle(
                      color: FemmeColors.paper,
                      fontSize: 16,
                      height: 1.35,
                    ),
                  ),
                ),
              ),
            AttachedFiles(paths: files, size: 88, alignEnd: true),
            if (line != null)
              GestureDetector(
                behavior: HitTestBehavior.opaque,
                onTap: red ? resend : null,
                child: Padding(
                  padding: EdgeInsets.only(top: 4, bottom: red ? 8 : 0),
                  child: Text(
                    line,
                    style: mono(
                      context,
                      size: 12,
                      color: red ? FemmeColors.bad : muted(context),
                    ),
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

class PeerCard extends StatelessWidget {
  const PeerCard(this.item, {super.key});
  final Item item;

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final b = c.json(item);
    final from = b?['from'] as String? ?? '';
    final text = c.text(item, Femme.of(context).store.textOf);
    return Padding(
      padding: const EdgeInsets.fromLTRB(_side, 6, 48, 6),
      child: GestureDetector(
        onLongPress: () => messageMenu(context, text),
        child: Container(
          width: double.infinity,
          padding: const EdgeInsets.all(10),
          decoration: BoxDecoration(
            color: paper(context),
            border: Border.all(color: ink(context), width: 1.2),
            boxShadow: [
              BoxShadow(color: ink(context), offset: const Offset(2, 2)),
            ],
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                '✉ from @${from.isEmpty ? 'someone' : from}',
                style: mono(context, size: 12, color: muted(context)),
              ),
              const SizedBox(height: 4),
              Md(text),
            ],
          ),
        ),
      ),
    );
  }
}

class AssistantRow extends StatelessWidget {
  const AssistantRow(this.item, {super.key});
  final Item item;

  @override
  Widget build(BuildContext context) {
    final store = Femme.of(context).store;
    final c = ChatScope.of(context);
    final split = splitAttached(c.text(item, store.textOf));
    final live = item.status == ItemStatus.inProgress || store.streaming(item);
    return GestureDetector(
      onLongPress: () => messageMenu(context, split.text),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: _side, vertical: 6),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (split.text.isNotEmpty) Md(split.text),
            if (live) const Cursor(),
            AttachedFiles(paths: split.paths, wide: true),
            if (item.status == ItemStatus.failed)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Text(
                  'cut off',
                  style: mono(context, size: 12, color: FemmeColors.bad),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

/// A tappable muted line that opens to [child].
class FoldLine extends StatelessWidget {
  const FoldLine({
    super.key,
    required this.id,
    required this.label,
    required this.child,
    this.red = false,
  });
  final String id;
  final String label;
  final Widget Function() child;
  final bool red;

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final open = c.isOpen(id);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        GestureDetector(
          behavior: HitTestBehavior.opaque,
          onTap: () => c.toggle(id),
          child: Container(
            constraints: const BoxConstraints(minHeight: 36),
            padding: const EdgeInsets.symmetric(horizontal: _side, vertical: 8),
            child: Text(
              '${open ? '▾' : '▸'} $label',
              style: mono(
                context,
                size: 12,
                color: red ? FemmeColors.bad : muted(context),
              ),
            ),
          ),
        ),
        if (open) child(),
      ],
    );
  }
}

class ThinkingRow extends StatelessWidget {
  const ThinkingRow(this.item, {super.key, this.ms});
  final Item item;
  final int? ms;

  @override
  Widget build(BuildContext context) {
    final text = ChatScope.of(context)
        .text(item, Femme.of(context).store.textOf);
    final label = item.status == ItemStatus.inProgress
        ? 'thinking…'
        : ms == null
        ? 'thought'
        : 'thought for ${duration(ms!)}';
    return FoldLine(
      id: item.itemId,
      label: label,
      child: () => Padding(
        padding: const EdgeInsets.fromLTRB(_side + 12, 0, _side, 8),
        child: Text(
          text,
          style: TextStyle(
            fontSize: 14,
            fontStyle: FontStyle.italic,
            color: muted(context),
          ),
        ),
      ),
    );
  }
}

class PlanRow extends StatelessWidget {
  const PlanRow(this.item, this.all, {super.key});
  final Item item;
  final List<Item> all;

  @override
  Widget build(BuildContext context) {
    final text = ChatScope.of(context)
        .text(item, Femme.of(context).store.textOf);
    final ask = all.where((i) => i.itemId == '${item.itemId}:ask').firstOrNull;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: _side, vertical: 8),
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          border: Border.all(color: ink(context), width: 1.2),
          color: paper(context),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('plan', style: mono(context, size: 12, color: planPurple)),
            const SizedBox(height: 6),
            Md(text),
            if (ask != null) ...[
              const SizedBox(height: 10),
              QuestionBody(ask, all, inPlan: true),
            ],
          ],
        ),
      ),
    );
  }
}

/// A muted centred note.
class NoteLine extends StatelessWidget {
  const NoteLine(this.text, {super.key, this.red = false});
  final String text;
  final bool red;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 6),
    child: Center(
      child: Text(
        text,
        textAlign: TextAlign.center,
        style: mono(
          context,
          size: 12,
          color: red ? FemmeColors.bad : muted(context),
        ),
      ),
    ),
  );
}

class EventRow extends StatelessWidget {
  const EventRow(this.item, {super.key});
  final Item item;

  @override
  Widget build(BuildContext context) {
    final text = ChatScope.of(context)
        .text(item, Femme.of(context).store.textOf);
    final t = text.trim();
    final red = item.status == ItemStatus.failed;
    return GestureDetector(
      onLongPress: () => messageMenu(context, t),
      child: NoteLine(t.startsWith('◎') ? t : '· $t', red: red),
    );
  }
}

class ReadRow extends StatelessWidget {
  const ReadRow(this.entry, {super.key});
  final ReadEntry entry;

  @override
  Widget build(BuildContext context) {
    final text = ChatScope.of(context)
        .text(entry.event, Femme.of(context).store.textOf);
    return Padding(
      padding: const EdgeInsets.fromLTRB(_side, 2, _side, 2),
      child: Row(
        children: [
          Expanded(
            child: Container(
              height: 1,
              color: muted(context).withValues(alpha: 0.4),
            ),
          ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 8),
            child: Text(
              '↳ ${text.split('\n').first}',
              style: mono(context, size: 11, color: muted(context)),
            ),
          ),
        ],
      ),
    );
  }
}

class CompactionRow extends StatelessWidget {
  const CompactionRow(this.item, {super.key});
  final Item item;

  @override
  Widget build(BuildContext context) {
    final text = ChatScope.of(context)
        .text(item, Femme.of(context).store.textOf);
    return FoldLine(
      id: item.itemId,
      label: '· conversation compacted · summary',
      child: () => Padding(
        padding: const EdgeInsets.fromLTRB(_side, 0, _side, 8),
        child: Md(text, size: 14),
      ),
    );
  }
}

class FoldRow extends StatelessWidget {
  const FoldRow(this.entry, this.all, {super.key});
  final FoldEntry entry;
  final List<Item> all;

  @override
  Widget build(BuildContext context) => FoldLine(
    id: entry.key,
    label: entry.label,
    child: () => Padding(
      padding: const EdgeInsets.only(left: 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [for (final e in entry.inner) entryRow(e, all)],
      ),
    ),
  );
}
