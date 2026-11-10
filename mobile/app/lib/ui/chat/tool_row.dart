/// A tool call and its result as one row: `▸ Bash echo hi → hi`; tap opens input and output in
/// monospace; `Agent` calls open their subagent; a running foreground task can go to the
/// background.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/wire.dart';
import '../../net/rpc.dart';
import '../theme.dart';
import 'look.dart';
import 'markdown.dart';
import 'media.dart';
import 'subagent.dart';
import 'tool_summary.dart';
import 'turns.dart';

class ToolRow extends StatelessWidget {
  const ToolRow(this.entry, {super.key});
  final ToolEntry entry;

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final call = entry.call;
    final res = entry.result;
    final open = c.isOpen(entry.key);
    // Message bodies are fetched at once, tool bodies only when the row is opened.
    final callBody = call == null ? null : c.json(call, fetch: open);
    final resBody = res == null ? null : c.json(res, fetch: open);
    final line = toolLine(
      call: call,
      callBody: callBody,
      result: res,
      resultBody: resBody,
      endedTasks: c.endedTasks,
    );
    final isAgent = line.name == 'Agent' || line.name == 'Task';
    final hasSub = isAgent && call != null && c.parents.contains(call.itemId);
    final mutedC = muted(context);
    final nameColor = line.failed ? FemmeColors.bad : ink(context);

    final summary = Text.rich(
      TextSpan(
        children: [
          TextSpan(
            text: open ? '▾ ' : '▸ ',
            style: TextStyle(color: mutedC),
          ),
          TextSpan(
            text: line.name,
            style: TextStyle(fontWeight: FontWeight.w700, color: nameColor),
          ),
          if (line.hint.isNotEmpty)
            TextSpan(
              text: ' ${line.hint}',
              style: TextStyle(
                color: line.linkUrl != null ? FemmeColors.accent : mutedC,
              ),
            ),
          if (line.result != null && line.result!.isNotEmpty)
            TextSpan(
              text: ' → ${line.result}',
              style: TextStyle(color: line.failed ? FemmeColors.bad : mutedC),
            ),
          if (line.running)
            TextSpan(
              text: ' …',
              style: TextStyle(color: FemmeColors.neon),
            ),
          if (line.inBackground)
            TextSpan(
              text: ' · in background',
              style: TextStyle(color: mutedC),
            ),
        ],
      ),
      maxLines: open ? 4 : 1,
      overflow: TextOverflow.ellipsis,
      style: mono(context, size: 13),
    );

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        GestureDetector(
          behavior: HitTestBehavior.opaque,
          onTap: () => c.toggle(entry.key),
          onLongPress: line.linkUrl == null
              ? null
              : () => openLink(line.linkUrl),
          child: Container(
            constraints: const BoxConstraints(minHeight: 40),
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
            child: Row(
              children: [
                Expanded(child: summary),
                if (hasSub)
                  _Action('open ›', () {
                    Navigator.of(context).push(
                      CupertinoPageRoute<void>(
                        builder: (_) => SubagentScreen(
                          sid: c.sid,
                          callId: call.itemId,
                          title: line.hint.isEmpty ? 'agent' : line.hint,
                          controller: c,
                        ),
                      ),
                    );
                  }),
                if (line.backgroundable && call != null)
                  _Action('⇣ background', () async {
                    HapticFeedback.selectionClick();
                    final err = await c.run(
                      Req.background(c.sid, toolUseId: call.itemId),
                    );
                    if (err != null && context.mounted) {
                      toast(context, 'background: $err');
                    }
                  }),
              ],
            ),
          ),
        ),
        if (open)
          _Opened(
            callBody: callBody,
            resBody: resBody,
            line: line,
            entry: entry,
          ),
      ],
    );
  }
}

class _Action extends StatelessWidget {
  const _Action(this.label, this.onTap);
  final String label;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) => CupertinoButton(
    padding: const EdgeInsets.only(left: 8),
    minimumSize: const Size(44, 44),
    onPressed: onTap,
    child: Text(
      label,
      style: mono(context, size: 12, color: FemmeColors.accent),
    ),
  );
}

class _Opened extends StatelessWidget {
  const _Opened({
    required this.callBody,
    required this.resBody,
    required this.line,
    required this.entry,
  });
  final Json? callBody;
  final Json? resBody;
  final ToolLine line;
  final ToolEntry entry;

  @override
  Widget build(BuildContext context) {
    final input = inputText(callBody?['input']);
    final out = resBody?['output'];
    final outText = out is String ? out : entry.result?.preview ?? '';
    final waiting =
        (entry.call?.body == null &&
            entry.call?.blobHash != null &&
            callBody == null) ||
        (entry.result?.body == null &&
            entry.result?.blobHash != null &&
            resBody == null);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 0, 16, 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (waiting) const CupertinoActivityIndicator(),
          if (line.linkUrl != null)
            CupertinoButton(
              padding: EdgeInsets.zero,
              alignment: Alignment.centerLeft,
              onPressed: () => openLink(line.linkUrl),
              child: Text(
                'open ${line.linkUrl} ↗',
                style: mono(context, size: 12, color: FemmeColors.accent),
              ),
            ),
          if (input.isNotEmpty) CodeBlock(capped(input), label: 'input'),
          if (outText.isNotEmpty)
            CodeBlock(capped(outText), label: 'output', red: line.failed),
        ],
      ),
    );
  }
}

/// A monospace block, horizontally scrollable, long-press copies.
class CodeBlock extends StatelessWidget {
  const CodeBlock(this.text, {super.key, this.label, this.red = false});
  final String text;
  final String? label;
  final bool red;

  @override
  Widget build(BuildContext context) => GestureDetector(
    onLongPress: () {
      Clipboard.setData(ClipboardData(text: text));
      HapticFeedback.selectionClick();
      toast(context, 'copied');
    },
    child: Container(
      margin: const EdgeInsets.only(top: 6),
      decoration: BoxDecoration(
        color: panel(context),
        border: Border.all(
          color: red ? FemmeColors.bad : ink(context),
          width: 1.2,
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (label != null)
            Padding(
              padding: const EdgeInsets.fromLTRB(8, 4, 8, 0),
              child: Text(
                label!,
                style: mono(context, size: 11, color: muted(context)),
              ),
            ),
          SingleChildScrollView(
            scrollDirection: Axis.horizontal,
            padding: const EdgeInsets.all(8),
            child: Text(
              text,
              style: mono(
                context,
                size: 12,
                color: red ? FemmeColors.bad : null,
              ),
            ),
          ),
        ],
      ),
    ),
  );
}
