/// The chat screen of one session: header, transcript, the strips and the composer. Holds the
/// session's subscription while shown.
library;

import 'package:flutter/cupertino.dart';

import '../net/models.dart' hide Row;
import '../state/chat.dart';
import '../state/store.dart';
import 'chat/composer.dart';
import 'chat/header.dart';
import 'chat/look.dart';
import 'chat/media.dart';
import 'chat/question.dart';
import 'chat/strips.dart';
import 'chat/transcript.dart';
import 'chat/turns.dart';
import 'femme.dart';

export 'chat/header.dart' show showChatMenu;

void openChat(BuildContext context, String sid) =>
    Navigator.of(context)
        .push(CupertinoPageRoute<void>(builder: (_) => ChatScreen(sid: sid)));

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key, required this.sid});
  final String sid;

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  ChatController? _c;
  Store? _store;
  late final DateTime _openedAt = DateTime.now();
  bool _sheetChecked = false;

  ChatController get _ctrl => _c!;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (_c == null) {
      final f = Femme.of(context);
      _c = ChatController(widget.sid, f.link);
      _store = f.store..subscribe(widget.sid);
    }
  }

  @override
  void dispose() {
    _store?.unsubscribe(widget.sid);
    _c?.dispose();
    super.dispose();
  }

  /// When the chat opens on a pending question, it comes up as a sheet (once, within the
  /// first seconds while the transcript arrives).
  void _maybeShowSheet(Item? pending) {
    if (_sheetChecked) return;
    if (DateTime.now().difference(_openedAt) > const Duration(seconds: 4)) {
      _sheetChecked = true;
      return;
    }
    if (pending == null) return;
    _sheetChecked = true;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) showQuestionSheet(context, _ctrl, pending.itemId);
    });
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    if (f.store.isClosed(widget.sid)) return _closed(context, f.store);
    final s = f.store.sessions[widget.sid];
    final items = f.store.items(widget.sid);
    final main = [
      for (final i in items)
        if (i.parent == null) i,
    ];
    final pending = pendingQuestion(_ctrl, main, s);
    _maybeShowSheet(pending);
    final working = s?.status == SessionStatus.working;

    return CupertinoPageScaffold(
      navigationBar: chatNavBar(context, widget.sid),
      child: SafeArea(
        bottom: false,
        child: ChatScope(
          controller: _ctrl,
          child: Column(
            children: [
              SubtitleStrip(sid: widget.sid),
              Expanded(
                child: TranscriptView(
                  sid: widget.sid,
                  footer: [
                    if (s != null && s.needsYou && pending == null)
                      BlockedCard(sid: widget.sid),
                    if (working)
                      WorkingLine(
                        verb: workingVerb(main),
                        since: lastUserTs(main),
                      ),
                  ],
                ),
              ),
              GoalStrip(s?.goal),
              if (s != null && s.agent != Agent.shell) XpBar(s.context),
              SafeArea(top: false, child: Composer(sid: widget.sid)),
            ],
          ),
        ),
      ),
    );
  }

  /// The session closed while (or before) the chat showed: its transcript can still be read, the
  /// composer gives way to a Close action (the desktop's `session closed` card, `frame.rs`).
  Widget _closed(BuildContext context, Store store) {
    final s = store.session(widget.sid);
    final line = BorderSide(color: panel(context), width: 1);
    return CupertinoPageScaffold(
      navigationBar: CupertinoNavigationBar(
        middle: Text(
          s == null ? 'chat' : sessionTitle(s),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
        ),
      ),
      child: SafeArea(
        bottom: false,
        child: ChatScope(
          controller: _ctrl,
          child: Column(
            children: [
              Container(
                width: double.infinity,
                decoration: BoxDecoration(border: Border(bottom: line)),
                padding: const EdgeInsets.all(8),
                child: Text(
                  'session closed',
                  style: mono(context, size: 12, color: muted(context)),
                ),
              ),
              Expanded(child: TranscriptView(sid: widget.sid)),
              SafeArea(
                top: false,
                child: Container(
                  decoration: BoxDecoration(border: Border(top: line)),
                  padding: const EdgeInsets.fromLTRB(12, 4, 4, 4),
                  child: Row(
                    children: [
                      Expanded(
                        child: Text(
                          'this session has ended',
                          style: TextStyle(color: muted(context)),
                        ),
                      ),
                      CupertinoButton(
                        onPressed: () => Navigator.of(context).maybePop(),
                        child: const Text('Close'),
                      ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
