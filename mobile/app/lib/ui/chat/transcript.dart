/// The transcript list: newest at the bottom (a reversed list, so it stays at the end while
/// text streams and keeps its place when older rows load at the top), a `↓` pill when
/// scrolled up, older history on reaching the top.
library;

import 'package:flutter/cupertino.dart';

import '../../net/models.dart' hide Row;
import '../../net/wire.dart';
import '../../state/chat.dart';
import '../femme.dart';
import '../theme.dart';
import 'look.dart';
import 'media.dart';
import 'rows.dart';
import 'subagent.dart';
import 'turns.dart';

const motion = Cubic(.32, .72, 0, 1);

class TranscriptView extends StatefulWidget {
  const TranscriptView({
    super.key,
    required this.sid,
    this.parent,
    this.footer = const [],
    this.empty = 'Nothing here yet.',
  });
  final String sid;

  /// Null for the main thread, else the `Agent` call id whose subagent this shows.
  final String? parent;

  /// Rows below the last item (the working line, a blocked card).
  final List<Widget> footer;
  final String empty;

  @override
  State<TranscriptView> createState() => _TranscriptViewState();
}

class _TranscriptViewState extends State<TranscriptView> {
  final _scroll = ScrollController();
  bool _away = false;
  int _seen = 0;
  int _count = 0;
  bool _loading = false;
  String? _olderError;
  bool _checkedFill = false;

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_onScroll);
  }

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  void _onScroll() {
    final p = _scroll.position;
    final away = p.pixels > 150;
    if (away != _away) {
      setState(() {
        _away = away;
        _seen = _count;
      });
    }
    if (p.extentAfter < 400) _loadOlder();
  }

  Future<void> _loadOlder() async {
    if (widget.parent != null || _loading || _olderError != null) return;
    final store = Femme.of(context).store;
    if (!store.hasOlder(widget.sid) || store.items(widget.sid).isEmpty) return;
    setState(() => _loading = true);
    try {
      await store.loadOlder(widget.sid);
    } catch (e) {
      _olderError = errorText(e);
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  void _toEnd() {
    _scroll.animateTo(
      0,
      duration: const Duration(milliseconds: 200),
      curve: motion,
    );
  }

  /// A list too short to scroll never reaches "the top" by scrolling: ask once after layout.
  void _fillCheck() {
    if (_checkedFill) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted || !_scroll.hasClients) return;
      if (_scroll.position.maxScrollExtent <= 0) {
        _checkedFill = Femme.of(context).store.items(widget.sid).isNotEmpty;
        _loadOlder();
      } else {
        _checkedFill = true;
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final store = Femme.of(context).store;
    final c = ChatScope.of(context);
    final all = store.items(widget.sid);
    Json? body(Item i) => c.json(i, fetch: false);
    c.endedTasks = endedTasksOf(all, body);
    c.parents = {
      for (final i in all)
        if (i.parent != null) i.parent!,
    };
    final view = [
      for (final i in all)
        if (i.parent == widget.parent) i,
    ];
    final entries = layout(view, body: body, endedTasks: c.endedTasks);
    final chips = agentChips(all, body);
    _count = entries.length;
    if (widget.parent == null) _fillCheck();

    final rows = <Widget>[
      _olderHeader(
        context,
        store.hasOlder(widget.sid) && widget.parent == null,
      ),
      if (entries.isEmpty && widget.footer.isEmpty)
        Padding(
          padding: const EdgeInsets.all(32),
          child: Center(
            child: Text(widget.empty, style: TextStyle(color: muted(context))),
          ),
        ),
      for (final e in entries)
        KeyedSubtree(key: ValueKey(e.key), child: entryRow(e, all)),
      ...widget.footer,
      const SizedBox(height: 8),
    ];
    final keyIndex = <Object, int>{
      for (var i = 0; i < rows.length; i++)
        if (rows[i].key != null) rows[i].key!: rows.length - 1 - i,
    };

    final list = Stack(
      children: [
        CupertinoScrollbar(
          controller: _scroll,
          child: ListView.builder(
            controller: _scroll,
            reverse: true,
            keyboardDismissBehavior: ScrollViewKeyboardDismissBehavior.onDrag,
            itemCount: rows.length,
            findChildIndexCallback: (k) => keyIndex[k],
            itemBuilder: (_, i) => rows[rows.length - 1 - i],
          ),
        ),
        if (_away)
          Positioned(
            right: 16,
            bottom: 12,
            child: GestureDetector(
              onTap: _toEnd,
              child: Container(
                constraints: const BoxConstraints(minHeight: 44, minWidth: 44),
                padding: const EdgeInsets.symmetric(
                  horizontal: 12,
                  vertical: 8,
                ),
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: paper(context),
                  border: Border.all(color: ink(context), width: 1.2),
                  boxShadow: [
                    BoxShadow(color: ink(context), offset: const Offset(2, 2)),
                  ],
                ),
                child: Text(
                  _count > _seen ? '↓ new' : '↓',
                  style: mono(
                    context,
                    size: 14,
                    color: _count > _seen ? FemmeColors.accent : null,
                  ),
                ),
              ),
            ),
          ),
      ],
    );
    if (chips.isEmpty && widget.parent == null) return list;
    return Column(
      children: [
        AgentStrip(chips: chips, view: widget.parent),
        Expanded(child: list),
      ],
    );
  }

  Widget _olderHeader(BuildContext context, bool more) {
    if (_loading) {
      return const Padding(
        padding: EdgeInsets.all(12),
        child: CupertinoActivityIndicator(),
      );
    }
    if (_olderError != null) {
      return CupertinoButton(
        onPressed: () {
          setState(() => _olderError = null);
          _loadOlder();
        },
        child: Text(
          'older messages failed: $_olderError · retry',
          textAlign: TextAlign.center,
          style: mono(context, size: 12, color: FemmeColors.bad),
        ),
      );
    }
    if (!more) return const SizedBox(height: 4);
    return CupertinoButton(
      onPressed: _loadOlder,
      child: Text(
        'older messages',
        style: mono(context, size: 12, color: muted(context)),
      ),
    );
  }
}
