/// The composer: a growing text field, `+` for files, prompt history, the `/` menu with the
/// `/model` picker, Stop while the agent works, and send (queue when idle, steer while working,
/// long-press = interrupt then send). Drafts are kept per chat.
library;

import 'dart:async';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/link.dart';
import '../../net/models.dart' hide Row;
import '../../net/rpc.dart';
import '../../state/chat.dart';
import '../../state/store.dart';
import '../femme.dart';
import '../theme.dart';
import 'attached.dart';
import 'attachments.dart';
import 'look.dart';
import 'media.dart';
import 'slash.dart';

/// The session's earlier prompts, newest first, without `Attached:` lines, no repeats.
List<String> promptHistory(List<Item> items, {int max = 20}) {
  final out = <String>[];
  for (final i in items.reversed) {
    if (i.kind != ItemKind.userMessage || i.parent != null) continue;
    final t = splitAttached(i.text).text.trim();
    if (t.isEmpty || t.startsWith(compactionPrefix) || out.contains(t)) {
      continue;
    }
    out.add(t);
    if (out.length >= max) break;
  }
  return out;
}

/// Whether stop takes the prompt back (the desktop's Esc rule, Claude Code's own): the newest
/// main-thread row that is not a note is still the user's message, so nothing of a reply has shown.
bool takesBack(List<Item> items) {
  for (final i in items.reversed) {
    if (i.parent != null ||
        i.kind == ItemKind.event ||
        i.kind == ItemKind.turnStart) {
      continue;
    }
    return i.kind == ItemKind.userMessage && i.status != ItemStatus.failed;
  }
  return false;
}

class Composer extends StatefulWidget {
  const Composer({super.key, required this.sid});
  final String sid;

  @override
  State<Composer> createState() => _ComposerState();
}

class _ComposerState extends State<Composer> {
  final _text = TextEditingController();
  final _focus = FocusNode();
  late final DraftFiles _files;
  late final Store _store;
  late final Requester _link;
  bool _hasFiles = false;
  Timer? _save;
  bool _loaded = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (!_hasFiles) {
      _hasFiles = true;
      final f = Femme.of(context);
      _store = f.store;
      _link = f.link;
      _files = DraftFiles(f.link)..addListener(_rebuild);
    }
  }

  void _rebuild() {
    if (mounted) setState(() {});
  }

  @override
  void initState() {
    super.initState();
    _text.addListener(_onText);
    ChatController.loadDraft(widget.sid).then((d) {
      if (!mounted) return;
      if (_text.text.isEmpty && d.isNotEmpty) _text.text = d;
      _loaded = true;
    });
  }

  @override
  void dispose() {
    _save?.cancel();
    if (_loaded) ChatController.saveDraft(widget.sid, _text.text);
    _text.dispose();
    _focus.dispose();
    _files.dispose();
    super.dispose();
  }

  void _onText() {
    setState(() {});
    if (!_loaded) return;
    _save?.cancel();
    _save = Timer(const Duration(milliseconds: 400), () {
      ChatController.saveDraft(widget.sid, _text.text);
    });
  }

  void _setText(String t) {
    _text.value = TextEditingValue(
      text: t,
      selection: TextSelection.collapsed(offset: t.length),
    );
    _focus.requestFocus();
  }

  Session? get _session => Femme.of(context).store.sessions[widget.sid];
  bool get _working => _session?.status == SessionStatus.working;

  Future<void> _sendText(
    String text,
    PromptHow how, {
    List<DraftFile> files = const [],
  }) async {
    final store = _store;
    try {
      await sendWithFiles(
        store,
        _link,
        widget.sid,
        text,
        how: how,
        sent: files,
      );
      // The daemon stores no message for a `/command`: drop its bubble once accepted.
      if (text.startsWith('/')) {
        for (final o in store.outbox) {
          if (o.sid == widget.sid && o.text == text) store.discard(o.op);
        }
      }
    } catch (_) {
      // the bubble shows the failure and offers retry
    }
  }

  Future<void> _send({bool interrupt = false}) async {
    final text = _text.text.trimRight();
    if (text.trim().isEmpty && _files.isEmpty) return;
    if (_files.uploading) return toast(context, 'still uploading: wait');
    if (_files.failed) {
      return toast(context, 'an upload failed: retry or remove it');
    }
    if (_session?.agent == Agent.shell) {
      return toast(context, 'this is a shell: use the terminal');
    }
    final how = interrupt
        ? PromptHow.interruptThenSend
        : text.startsWith('/') || !_working
        ? PromptHow.queue
        : PromptHow.steer;
    interrupt ? HapticFeedback.mediumImpact() : HapticFeedback.lightImpact();
    // Kept with their bytes: an upload the box has swept since goes up again.
    final sent = [
      for (final f in _files.files)
        if (f.state == UploadState.ready) f,
    ];
    _files.clear();
    _text.clear();
    ChatController.saveDraft(widget.sid, '');
    await _sendText(text, how, files: sent);
  }

  Future<void> _pickModel(ModelChoice m, String? effort) async {
    HapticFeedback.lightImpact();
    _text.clear();
    for (final p in modelPrompts(m, effort)) {
      await _sendText(p, PromptHow.queue);
    }
  }

  Future<void> _interrupt() async {
    HapticFeedback.mediumImpact();
    final chat = ChatScope.of(context);
    final items = Femme.of(context).store.items(widget.sid);
    if (_session?.agent == Agent.claude && takesBack(items)) {
      try {
        // the prompt comes back to edit; a daemon that only interrupted answers no text
        final text = await chat.takeBack();
        if (text != null && mounted) {
          final had = _text.text;
          _setText(had.isEmpty ? text : '$text\n$had');
        }
        return;
      } catch (_) {
        // an older daemon does not know the request: interrupt as before
      }
    }
    final err = await chat.run(Req.interrupt(widget.sid));
    if (err != null && mounted) toast(context, 'interrupt failed: $err');
  }

  Future<void> _attach() async {
    final notes = await pickInto(context, _files);
    if (notes.isNotEmpty && mounted) toast(context, notes.join('\n'));
  }

  Future<void> _history() async {
    final items = Femme.of(context).store.items(widget.sid);
    final h = promptHistory(items);
    if (h.isEmpty) return toast(context, 'no earlier prompts');
    final pick = await showCupertinoModalPopup<String>(
      context: context,
      builder: (ctx) => CupertinoActionSheet(
        title: const Text('Earlier prompts'),
        actions: [
          for (final t in h)
            CupertinoActionSheetAction(
              onPressed: () => Navigator.pop(ctx, t),
              child: Text(
                t,
                maxLines: 2,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(fontSize: 15),
              ),
            ),
        ],
        cancelButton: CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(ctx),
          child: const Text('Cancel'),
        ),
      ),
    );
    if (pick != null) _setText(pick);
  }

  String _placeholder(Femme f) {
    final s = _session;
    if (!f.link.isOnline) return 'not connected';
    if (s == null) return 'no session';
    if (s.agent == Agent.shell) return 'this is a shell';
    return 'message ${modelEffort(s)}';
  }

  Widget _suggestions(BuildContext context) {
    final text = _text.text;
    final step = modelStep(text);
    final List<(String, String?, VoidCallback)> rows = switch (step) {
      PickModel(:final choices) => [
        for (final m in choices)
          (
            'model ${m.label}',
            m.claude ? 'then pick an effort' : 'switches now',
            () =>
                m.claude ? _setText('/model ${m.name} ') : _pickModel(m, null),
          ),
      ],
      PickEffort(:final model, :final choices) => [
        for (final e in choices)
          ('effort $e', model.label, () => _pickModel(model, e)),
      ],
      null => [
        for (final n in matchSlash(text, ChatScope.of(context).commands))
          ('/$n', null, () => _setText(n == 'model' ? '/model ' : '/$n ')),
      ],
    };
    if (rows.isEmpty) return const SizedBox.shrink();
    return Container(
      constraints: const BoxConstraints(maxHeight: 280),
      margin: const EdgeInsets.fromLTRB(12, 0, 12, 6),
      decoration: BoxDecoration(
        color: CupertinoColors.secondarySystemGroupedBackground.resolveFrom(
          context,
        ),
        borderRadius: BorderRadius.circular(cardRadius),
        boxShadow: softShadow,
      ),
      clipBehavior: Clip.antiAlias,
      child: ListView(
        key: const ValueKey('slash-popup'),
        shrinkWrap: true,
        padding: EdgeInsets.zero,
        children: [
          for (final (i, (label, sub, tap)) in rows.indexed) ...[
            if (i > 0)
              Container(
                margin: const EdgeInsets.only(left: 12),
                height: hairline(context),
                color: separator(context),
              ),
            GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTap: () {
                HapticFeedback.selectionClick();
                tap();
              },
              child: Container(
                constraints: const BoxConstraints(minHeight: 44),
                padding: const EdgeInsets.symmetric(
                  horizontal: 12,
                  vertical: 10,
                ),
                child: Row(
                  children: [
                    Expanded(
                      child: Text(label, style: mono(context, size: 14)),
                    ),
                    if (sub != null)
                      Text(
                        sub,
                        style: mono(context, size: 11, color: muted(context)),
                      ),
                  ],
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final working = _working;
    final shell = _session?.agent == Agent.shell;
    final canSend = !shell && (_text.text.trim().isNotEmpty || !_files.isEmpty);
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        _suggestions(context),
        FileChips(_files),
        Container(
          decoration: BoxDecoration(
            color: paper(context),
            border: Border(
              top: BorderSide(
                color: separator(context),
                width: hairline(context),
              ),
            ),
          ),
          padding: const EdgeInsets.fromLTRB(4, 6, 4, 6),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.end,
            children: [
              _IconBtn(CupertinoIcons.add, shell ? null : _attach, 'attach'),
              _IconBtn(CupertinoIcons.clock, _history, 'earlier prompts'),
              Expanded(
                child: CupertinoTextField(
                  controller: _text,
                  focusNode: _focus,
                  minLines: 1,
                  maxLines: 6,
                  enabled: !shell,
                  keyboardType: TextInputType.multiline,
                  textCapitalization: TextCapitalization.sentences,
                  placeholder: _placeholder(f),
                  padding: const EdgeInsets.symmetric(
                    horizontal: 14,
                    vertical: 9,
                  ),
                  style: TextStyle(fontSize: 16, color: ink(context)),
                  decoration: BoxDecoration(
                    color: paper(context),
                    borderRadius: BorderRadius.circular(composerRadius),
                    border: Border.all(
                      color: separator(context),
                      width: hairline(context),
                    ),
                  ),
                ),
              ),
              if (working)
                _IconBtn(
                  CupertinoIcons.stop_fill,
                  _interrupt,
                  'interrupt',
                  color: FemmeColors.bad,
                ),
              GestureDetector(
                onLongPress: canSend ? () => _send(interrupt: true) : null,
                child: _IconBtn(
                  working
                      ? CupertinoIcons.arrow_turn_down_right
                      : CupertinoIcons.arrow_up_circle_fill,
                  canSend ? () => _send() : null,
                  working ? 'steer' : 'send',
                  color: FemmeColors.accent,
                  size: 30,
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }
}

class _IconBtn extends StatelessWidget {
  const _IconBtn(
    this.icon,
    this.onTap,
    this.label, {
    this.color,
    this.size = 24,
  });
  final IconData icon;
  final VoidCallback? onTap;
  final String label;
  final Color? color;
  final double size;

  @override
  Widget build(BuildContext context) => Semantics(
    label: label,
    button: true,
    excludeSemantics: true,
    child: CupertinoButton(
      padding: EdgeInsets.zero,
      minimumSize: const Size(44, 44),
      onPressed: onTap,
      child: Icon(
        icon,
        size: size,
        color: onTap == null ? muted(context) : (color ?? ink(context)),
      ),
    ),
  );
}
