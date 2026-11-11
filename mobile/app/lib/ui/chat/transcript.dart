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

  /// Contexts of the rows built now, by entry key; and every row's list index (0 = newest).
  final _built = <String, BuildContext>{};
  var _indexOf = <String, int>{};

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

  /// Scrolls to the row [key] (a read mark's message): straight there when it is built, else
  /// a viewport at a time towards it until it is.
  Future<void> _jumpTo(String key) async {
    final want = _indexOf[key];
    if (want == null) {
      toast(context, 'that message is not loaded');
      return;
    }
    for (var n = 0; n < 40 && mounted && _scroll.hasClients; n++) {
      final ctx = _built[key];
      if (ctx != null && ctx.mounted) {
        await Scrollable.ensureVisible(
          ctx,
          alignment: 0.5,
          duration: const Duration(milliseconds: 300),
          curve: motion,
        );
        return;
      }
      final p = _scroll.position;
      final shown = [
        for (final k in _built.keys)
          if (_indexOf[k] case final int i) i,
      ];
      // the list is reversed: a higher index sits further up, at more pixels
      final up = shown.isEmpty || want > shown.reduce((a, b) => a > b ? a : b);
      final step = p.viewportDimension * 0.8 * (up ? 1 : -1);
      final to = (p.pixels + step).clamp(p.minScrollExtent, p.maxScrollExtent);
      if (to == p.pixels) return;
      _scroll.jumpTo(to);
      await WidgetsBinding.instance.endOfFrame;
    }
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
        KeyedSubtree(
          key: ValueKey(e.key),
          child: _Anchor(
            e.key,
            _built,
            child: FadeIn(ts: entryTs(e), child: entryRow(e, all)),
          ),
        ),
      ...widget.footer,
      const SizedBox(height: 8),
    ];
    final keyIndex = <Object, int>{
      for (var i = 0; i < rows.length; i++)
        if (rows[i].key != null) rows[i].key!: rows.length - 1 - i,
    };
    _indexOf = {
      for (final MapEntry(:key, :value) in keyIndex.entries)
        if (key is ValueKey<String>) key.value: value,
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
            child: Semantics(
              label: _count > _seen
                  ? 'new messages, scroll to latest'
                  : 'scroll to latest',
              button: true,
              excludeSemantics: true,
              child: GestureDetector(
                onTap: _toEnd,
                child: Container(
                  constraints: const BoxConstraints(
                    minHeight: 44,
                    minWidth: 44,
                  ),
                  padding: const EdgeInsets.symmetric(
                    horizontal: 12,
                    vertical: 8,
                  ),
                  alignment: Alignment.center,
                  decoration: BoxDecoration(
                    color: CupertinoColors.secondarySystemGroupedBackground
                        .resolveFrom(context),
                    borderRadius: BorderRadius.circular(999),
                    boxShadow: softShadow,
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
          ),
      ],
    );
    final jumpable = TranscriptJump(jump: _jumpTo, child: list);
    if (chips.isEmpty && widget.parent == null) return jumpable;
    return Column(
      children: [
        AgentStrip(chips: chips, view: widget.parent),
        Expanded(child: jumpable),
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

/// Hands the rows a way to scroll the transcript to another row (a read mark to its message).
class TranscriptJump extends InheritedWidget {
  const TranscriptJump({super.key, required this.jump, required super.child});
  final Future<void> Function(String key) jump;

  static TranscriptJump? of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<TranscriptJump>();

  @override
  bool updateShouldNotify(TranscriptJump old) => false;
}

/// Notes its row's context in [built] while the row is built (the list builds lazily).
class _Anchor extends StatefulWidget {
  const _Anchor(this.id, this.built, {required this.child});
  final String id;
  final Map<String, BuildContext> built;
  final Widget child;

  @override
  State<_Anchor> createState() => _AnchorState();
}

class _AnchorState extends State<_Anchor> {
  @override
  void initState() {
    super.initState();
    widget.built[widget.id] = context;
  }

  @override
  void didUpdateWidget(_Anchor old) {
    super.didUpdateWidget(old);
    if (old.id != widget.id) {
      if (identical(old.built[old.id], context)) old.built.remove(old.id);
      widget.built[widget.id] = context;
    }
  }

  @override
  void dispose() {
    if (identical(widget.built[widget.id], context)) {
      widget.built.remove(widget.id);
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => widget.child;
}

/// A row made less than a second ago fades in and rises 4 px when first built (desktop
/// rows.rs fade_in, 180 ms); history, and rows built again on scrolling, show as they are.
class FadeIn extends StatefulWidget {
  const FadeIn({super.key, required this.ts, required this.child});

  /// When the row's item was made, unix ms.
  final int ts;
  final Widget child;

  static const fresh = Duration(seconds: 1);
  static const length = Duration(milliseconds: 180);

  @override
  State<FadeIn> createState() => _FadeInState();
}

class _FadeInState extends State<FadeIn> {
  late final bool _fresh =
      DateTime.now().millisecondsSinceEpoch - widget.ts <
      FadeIn.fresh.inMilliseconds;

  @override
  Widget build(BuildContext context) {
    if (!_fresh || MediaQuery.disableAnimationsOf(context)) {
      return widget.child;
    }
    return TweenAnimationBuilder<double>(
      tween: Tween(begin: 0, end: 1),
      duration: FadeIn.length,
      curve: Curves.easeOut,
      child: widget.child,
      builder: (_, t, child) => Opacity(
        opacity: t,
        child: Transform.translate(
          offset: Offset(0, 4 * (1 - t)),
          child: child,
        ),
      ),
    );
  }
}
