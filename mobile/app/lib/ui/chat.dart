import 'package:flutter/cupertino.dart';

import '../net/models.dart';
import 'femme.dart';
import 'theme.dart';

void openChat(BuildContext context, String sid) => Navigator.of(context)
    .push(CupertinoPageRoute<void>(builder: (_) => ChatScreen(sid: sid)));

/// Placeholder chat screen: the session's items as plain lines and a one-line composer. Holds
/// the session's subscription while it is shown.
class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key, required this.sid});
  final String sid;

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final _text = TextEditingController();
  late final _store = Femme.of(context).store;
  bool _subscribed = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (!_subscribed) {
      _subscribed = true;
      _store.subscribe(widget.sid);
    }
  }

  @override
  void dispose() {
    _store.unsubscribe(widget.sid);
    _text.dispose();
    super.dispose();
  }

  void _send() {
    final t = _text.text.trim();
    if (t.isEmpty) return;
    _text.clear();
    // A failure shows on the bubble (the outbox entry turns failed); nothing to do here.
    _store.send(widget.sid, t).ignore();
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final session = f.store.sessions[widget.sid];
    final items = f.store
        .items(widget.sid)
        .where((i) => i.parent == null)
        .toList();
    return CupertinoPageScaffold(
      navigationBar: CupertinoNavigationBar(
        middle: Text(session?.title ?? widget.sid),
      ),
      child: SafeArea(
        child: Column(
          children: [
            Expanded(
              child: ListView.builder(
                reverse: true,
                itemCount: items.length,
                itemBuilder: (context, i) =>
                    _line(context, items[items.length - 1 - i]),
              ),
            ),
            Padding(
              padding: const EdgeInsets.all(8),
              child: CupertinoTextField(
                controller: _text,
                placeholder: 'Message',
                onSubmitted: (_) => _send(),
                suffix: CupertinoButton(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  onPressed: _send,
                  child: const Icon(CupertinoIcons.arrow_up_circle_fill),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _line(BuildContext context, Item item) {
    final store = Femme.of(context).store;
    final mine = item.kind == ItemKind.userMessage;
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final text = item.kind == ItemKind.question
        ? '? ${item.question?.prompt ?? item.preview}'
        : store.textOf(item);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 4),
      child: Column(
        crossAxisAlignment: mine
            ? CrossAxisAlignment.end
            : CrossAxisAlignment.start,
        children: [
          Text(
            text,
            maxLines: 12,
            overflow: TextOverflow.ellipsis,
            textAlign: mine ? TextAlign.right : TextAlign.left,
          ),
          Text(
            '${item.kind.name} · ${item.status.name}',
            style: TextStyle(fontSize: 11, color: muted),
          ),
        ],
      ),
    );
  }
}
