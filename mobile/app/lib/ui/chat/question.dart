/// Questions and permission prompts (BODIES.md §3.7): the inline card, the bottom sheet, the
/// answered summary, and the card of a session blocked with no question held.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/models.dart' hide Row;
import '../../state/chat.dart';
import '../femme.dart';
import '../terminal.dart';
import '../theme.dart';
import 'look.dart';
import 'media.dart';
import 'tool_row.dart';
import 'tool_summary.dart';

/// The question in [item]'s body (inline, or its fetched blob).
Question? questionOf(ChatController c, Item item) {
  if (item.kind != ItemKind.question) return null;
  final j = c.json(item);
  if (j == null || j['request_id'] == null) return null;
  return Question.fromJson(j);
}

/// Open: pending, not answered, no reason. Codex and pi leave answered questions `Pending`
/// (BODIES.md surprise 6), so for them the session must still be blocked too.
bool questionOpen(Item item, Question? q, Session? s) {
  if (q == null || item.status != ItemStatus.pending) return false;
  if (q.answered != null || q.reason != null) return false;
  if (s != null && s.agent != Agent.claude && !s.needsYou) return false;
  return true;
}

/// `Allow` / `Deny` on a `<call>:ask` item.
bool isPermission(Item item, Question q) =>
    item.itemId.endsWith(':ask') &&
    q.options.length == 2 &&
    q.options[0] == 'Allow' &&
    q.options[1] == 'Deny';

/// An approval of a plan whose `Plan` row is held: the plan row draws it.
bool isPlanApproval(Item item, List<Item> all) {
  if (!item.itemId.endsWith(':ask')) return false;
  final id = item.itemId.substring(0, item.itemId.length - 4);
  return all.any((i) => i.itemId == id && i.kind == ItemKind.plan);
}

/// What was answered: labels joined, the text, or `cancelled`.
String answerLabel(Question q) {
  final a = q.answered;
  if (a == null) return '';
  if (a.isCancel) return 'cancelled';
  if (a.text != null) return a.text!;
  String at(int i) =>
      i >= 0 && i < q.options.length ? _label(q.options[i]) : '#$i';
  if (a.option != null) return at(a.option!);
  return (a.options ?? const []).map(at).join(', ');
}

String _label(String option) {
  final i = option.indexOf(' — ');
  return i < 0 ? option : option.substring(0, i);
}

class QuestionRow extends StatelessWidget {
  const QuestionRow(this.item, this.all, {super.key});
  final Item item;
  final List<Item> all;

  @override
  Widget build(BuildContext context) {
    if (isPlanApproval(item, all)) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
      child: Container(
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          color: CupertinoColors.secondarySystemBackground.resolveFrom(context),
          borderRadius: BorderRadius.circular(cardRadius),
        ),
        child: QuestionBody(item, all),
      ),
    );
  }
}

/// Prompt, options and answer controls of one question; the answered summary once closed.
class QuestionBody extends StatefulWidget {
  const QuestionBody(this.item, this.all, {super.key, this.inPlan = false});
  final Item item;
  final List<Item> all;
  final bool inPlan;

  @override
  State<QuestionBody> createState() => _QuestionBodyState();
}

class _QuestionBodyState extends State<QuestionBody> {
  final _ticks = <int>{};
  final _text = TextEditingController();

  @override
  void dispose() {
    _text.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final store = Femme.of(context).store;
    final item = widget.item;
    final q = questionOf(c, item);
    final mutedC = muted(context);
    if (q == null) {
      return Text('? ${item.preview}', style: TextStyle(color: mutedC));
    }
    final open = questionOpen(item, q, store.sessions[item.sid]);
    final st = c.answers[item.itemId];
    final sending = st?.sending == true;
    final permission = isPermission(item, q);

    void send(Answer a) {
      HapticFeedback.lightImpact();
      c.answer(item, q.requestId, a);
    }

    final children = <Widget>[];
    if (!widget.inPlan) {
      children.add(
        Text(
          q.prompt,
          style: TextStyle(
            fontSize: 17,
            fontWeight: FontWeight.w600,
            color: ink(context),
          ),
        ),
      );
    }
    if (permission) {
      final callId = item.itemId.substring(0, item.itemId.length - 4);
      final call = widget.all.where((i) => i.itemId == callId).firstOrNull;
      final body = call == null ? null : c.json(call);
      if (body != null) {
        final input = inputText(body['input']);
        children.add(
          CodeBlock(
            capped(input.isEmpty ? '${body['name']}' : input, maxLines: 40),
            label: '${body['name'] ?? 'tool'}',
          ),
        );
      }
    }
    if (!open) {
      final why = q.reason;
      children.add(
        Padding(
          padding: const EdgeInsets.only(top: 6),
          child: Text(
            why != null
                ? 'not answered: $why'
                : q.answered != null
                ? '→ ${answerLabel(q)}'
                : 'answered',
            style: mono(
              context,
              size: 13,
              color: why != null ? FemmeColors.bad : mutedC,
            ),
          ),
        ),
      );
      return Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: children,
      );
    }

    children.add(const SizedBox(height: 10));
    for (var i = 0; i < q.options.length; i++) {
      final o = q.options[i];
      final dash = o.indexOf(' — ');
      final label = dash < 0 ? o : o.substring(0, dash);
      final desc = dash < 0 ? null : o.substring(dash + 3);
      final ticked = _ticks.contains(i);
      children.add(
        Padding(
          padding: const EdgeInsets.only(bottom: 8),
          child: _OptionButton(
            label: q.multi ? '${ticked ? '☑' : '☐'}  $label' : label,
            detail: desc,
            primary: !q.multi && i == 0,
            danger: permission && i == 1,
            onTap: sending
                ? null
                : q.multi
                ? () =>
                      setState(() => ticked ? _ticks.remove(i) : _ticks.add(i))
                : () => send(Answer.option(i)),
          ),
        ),
      );
    }
    if (q.multi) {
      children.add(
        CupertinoButton.filled(
          onPressed: sending || _ticks.isEmpty
              ? null
              : () => send(Answer.options(_ticks.toList()..sort())),
          child: const Text('Send'),
        ),
      );
    }
    if (q.freeText) {
      children.add(
        Padding(
          padding: const EdgeInsets.only(top: 4),
          child: CupertinoTextField(
            controller: _text,
            enabled: !sending,
            minLines: 1,
            maxLines: 4,
            placeholder: permission || widget.inPlan
                ? 'or say what to do instead'
                : 'or type an answer',
            textInputAction: TextInputAction.send,
            onSubmitted: (t) {
              if (t.trim().isNotEmpty) send(Answer.text(t.trim()));
            },
            suffix: CupertinoButton(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              minimumSize: const Size(44, 44),
              onPressed: sending
                  ? null
                  : () {
                      final t = _text.text.trim();
                      if (t.isNotEmpty) send(Answer.text(t));
                    },
              child: const Icon(
                CupertinoIcons.arrow_up_circle_fill,
                semanticLabel: 'send answer',
              ),
            ),
          ),
        ),
      );
    }
    children.add(
      Row(
        children: [
          if (sending) ...[
            const CupertinoActivityIndicator(radius: 8),
            const SizedBox(width: 8),
            Text('sending…', style: mono(context, size: 12, color: mutedC)),
          ],
          if (st?.error != null)
            Expanded(
              child: Text(
                st!.error!,
                style: mono(context, size: 12, color: FemmeColors.bad),
              ),
            ),
          if (st?.error == null) const Spacer(),
          CupertinoButton(
            minimumSize: const Size(44, 44),
            onPressed: sending ? null : () => send(Answer.cancel),
            child: Text(permission ? 'Dismiss (deny)' : 'Cancel'),
          ),
        ],
      ),
    );
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: children,
    );
  }
}

class _OptionButton extends StatelessWidget {
  const _OptionButton({
    required this.label,
    this.detail,
    this.onTap,
    this.primary = false,
    this.danger = false,
  });
  final String label;
  final String? detail;
  final VoidCallback? onTap;
  final bool primary;
  final bool danger;

  @override
  Widget build(BuildContext context) {
    final fill = primary ? FemmeColors.accent : paper(context);
    final fg = primary
        ? FemmeColors.paper
        : (danger ? FemmeColors.bad : ink(context));
    return GestureDetector(
      onTap: onTap,
      child: Opacity(
        opacity: onTap == null ? 0.5 : 1,
        child: Container(
          constraints: const BoxConstraints(minHeight: 44),
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
          decoration: BoxDecoration(
            color: fill,
            borderRadius: BorderRadius.circular(cardRadius),
            border: Border.all(
              color: separator(context),
              width: hairline(context),
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                label,
                style: TextStyle(
                  fontSize: 16,
                  fontWeight: FontWeight.w600,
                  color: fg,
                ),
              ),
              if (detail != null && detail!.isNotEmpty)
                Text(
                  detail!,
                  style: TextStyle(
                    fontSize: 14,
                    color: primary ? fg : muted(context),
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

/// The first open question of the session, if any.
Item? pendingQuestion(ChatController c, List<Item> items, Session? s) {
  for (final i in items) {
    if (i.kind == ItemKind.question && questionOpen(i, questionOf(c, i), s)) {
      return i;
    }
  }
  return null;
}

/// The question as a bottom sheet; closes itself once it is answered.
Future<void> showQuestionSheet(
  BuildContext context,
  ChatController c,
  String itemId,
) => showCupertinoModalPopup<void>(
  context: context,
  builder: (ctx) => ChatScope(controller: c, child: _QuestionSheet(itemId)),
);

class _QuestionSheet extends StatefulWidget {
  const _QuestionSheet(this.itemId);
  final String itemId;
  @override
  State<_QuestionSheet> createState() => _QuestionSheetState();
}

class _QuestionSheetState extends State<_QuestionSheet> {
  bool _closing = false;

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final store = Femme.of(context).store;
    final all = store.items(c.sid);
    final item = all.where((i) => i.itemId == widget.itemId).firstOrNull;
    final open =
        item != null &&
        questionOpen(item, questionOf(c, item), store.sessions[c.sid]);
    if (!open && !_closing) {
      _closing = true;
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) Navigator.of(context).maybePop();
      });
    }
    final h = MediaQuery.of(context).size.height;
    return Container(
      constraints: BoxConstraints(maxHeight: h * 0.85),
      decoration: BoxDecoration(
        color: paper(context),
        border: Border(
          top: BorderSide(color: separator(context), width: hairline(context)),
        ),
      ),
      child: SafeArea(
        top: false,
        child: SingleChildScrollView(
          padding: EdgeInsets.fromLTRB(
            16,
            12,
            16,
            16 + MediaQuery.of(context).viewInsets.bottom,
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Center(
                child: Container(width: 36, height: 4, color: muted(context)),
              ),
              const SizedBox(height: 10),
              Text(
                store.sessions[c.sid] == null
                    ? 'needs you'
                    : '${sessionTitle(store.sessions[c.sid]!)} · needs you',
                style: mono(context, size: 12, color: muted(context)),
              ),
              const SizedBox(height: 6),
              if (item != null) QuestionBody(item, all),
            ],
          ),
        ),
      ),
    );
  }
}

/// A session blocked with no question held (a TUI dialog, a question not loaded): says so and
/// offers the terminal.
class BlockedCard extends StatelessWidget {
  const BlockedCard({super.key, required this.sid});
  final String sid;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
    child: Container(
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: FemmeColors.pause.withValues(alpha: 0.14),
        borderRadius: BorderRadius.circular(cardRadius),
      ),
      child: Row(
        children: [
          const StatusSquare(FemmeColors.pause),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              'The agent is waiting for you, but its question is not here. It may be a dialog on its screen.',
              style: TextStyle(fontSize: 15, color: ink(context)),
            ),
          ),
          CupertinoButton(
            minimumSize: const Size(44, 44),
            onPressed: () => Navigator.of(context).push(
              CupertinoPageRoute<void>(
                builder: (_) => TerminalScreen(sid: sid),
              ),
            ),
            child: const Text('Terminal'),
          ),
        ],
      ),
    ),
  );
}

class StatusSquare extends StatelessWidget {
  const StatusSquare(this.color, {super.key});
  final Color color;
  @override
  Widget build(BuildContext context) => Container(
    width: 10,
    height: 10,
    decoration: BoxDecoration(color: color, shape: BoxShape.circle),
  );
}
