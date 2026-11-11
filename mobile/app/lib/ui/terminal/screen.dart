/// The Terminal screen: the terminal panes of every live session (a segmented control when there
/// are several, oldest session first), opened on one session's first pane,
/// the grid, the hidden keyboard field and the extra-keys row; `+` opens a shell pane.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/models.dart' hide Row;
import '../../net/rpc.dart';
import '../../state/store.dart';
import '../../state/term.dart';
import '../femme.dart';
import '../theme.dart';
import 'extra_keys.dart';
import 'input.dart';
import 'keys.dart';
import 'surface.dart';

/// The terminal panes of [sid], by number. A headless agent's own pane stands for its
/// transcript and is not one.
List<Pane> terminalPanes(Store store, String sid) {
  final s = store.sessions[sid];
  return store.panes.values
      .where((p) => p.id.sid == sid)
      .where((p) => !(s?.mode == Mode.headless && p.kind == PaneKind.agent))
      .toList()
    ..sort((a, b) => a.id.no.compareTo(b.id.no));
}

/// The panes that get a tab: those of live sessions, oldest session first (a new one lands at
/// the right), then by number (desktop `net.rs` `tabs`). A headless agent's own pane is not one.
List<Pane> terminalTabs(Store store) {
  final v = <(Session, Pane)>[];
  for (final p in store.panes.values) {
    final s = store.sessions[p.id.sid];
    if (s == null || sessionEnded(store, s.id)) continue;
    if (s.mode == Mode.headless && p.kind == PaneKind.agent) continue;
    v.add((s, p));
  }
  v.sort((a, b) {
    final c = a.$1.created.compareTo(b.$1.created);
    if (c != 0) return c;
    final d = a.$1.id.compareTo(b.$1.id);
    return d != 0 ? d : a.$2.id.no.compareTo(b.$2.id.no);
  });
  return [for (final (_, p) in v) p];
}

/// The tab on show: [picked] if it is in [tabs], else the one that took its place (position
/// [ix], or the last), else null for no tabs (desktop `terminal/panel/tabs.rs` `on_show`).
int? onShow(List<Pane> tabs, PaneId? picked, int ix) {
  if (tabs.isEmpty) return null;
  final at = tabs.indexWhere((p) => p.id == picked);
  return at >= 0 ? at : ix.clamp(0, tabs.length - 1);
}

/// A pane's label: the program's title, else the pane's, else `<session> · <no>`; 28 chars.
String paneLabel(Store store, PaneId id, String? termTitle) {
  String? named(String? t) => t == null || t.isEmpty ? null : t;
  final l =
      named(termTitle) ??
      named(store.panes[id.key]?.title) ??
      '${store.sessions[id.sid]?.title ?? 'terminal'} · ${id.no}';
  return l.runes.length > 28 ? '${String.fromCharCodes(l.runes.take(28))}…' : l;
}

/// What closing [pane] asks of the box, or null when it is the agent's own pane.
Object? closeReq(Store store, PaneId pane) {
  final p = store.panes[pane.key];
  final s = store.sessions[pane.sid];
  if (p == null || s == null || p.kind == PaneKind.agent) return null;
  final first = terminalPanes(store, pane.sid).firstOrNull;
  return s.agent == Agent.shell && first?.id == pane
      ? Req.sessionClose(pane.sid)
      : Req.paneClose(pane);
}

/// Whether [sid] has ended: closed, or gone from the store (desktop `frame.rs` `centre_of`).
bool sessionEnded(Store store, String sid) {
  final s = store.sessions[sid];
  return s == null || s.closed != null || s.status == SessionStatus.closed;
}

/// Whether [pane] is its session's own terminal: the agent's pane of a TUI session, or the first
/// pane of a shell. Its program ending ends the session.
bool isMainPane(Store store, PaneId pane) {
  final s = store.sessions[pane.sid];
  if (s == null) return true;
  if (s.agent == Agent.shell) {
    final first = terminalPanes(store, pane.sid).firstOrNull;
    return first == null || first.id == pane;
  }
  return store.panes[pane.key]?.kind == PaneKind.agent;
}

/// The ended card's headline: `session closed` (desktop `Card::Closed`) when the session or its
/// own terminal ended, `pane closed` for another pane whose program ended.
String endedTitle(Store store, PaneId pane) =>
    sessionEnded(store, pane.sid) || isMainPane(store, pane)
    ? 'session closed'
    : 'pane closed';

class TerminalScreenBody extends StatefulWidget {
  const TerminalScreenBody({super.key, required this.sid, this.pane});
  final String sid;
  final int? pane;

  @override
  State<TerminalScreenBody> createState() => _TerminalScreenBodyState();
}

class _TerminalScreenBodyState extends State<TerminalScreenBody> {
  final _focus = FocusNode();
  final _mods = StickyMods();

  /// The pane picked, and its position among the tabs when it was last shown.
  PaneId? _picked;
  int _ix = 0;
  TermView? _view;
  int _bells = 0;
  String? _toast;
  bool _opening = false;

  @override
  void initState() {
    super.initState();
    final no = widget.pane;
    if (no != null) _picked = PaneId(widget.sid, no);
  }

  @override
  void dispose() {
    _view?.removeListener(_onView);
    _view?.dispose();
    _focus.dispose();
    super.dispose();
  }

  /// Attaches the pane on screen (only that one: every attached pane is a stream to feed).
  void _attach(PaneId id) {
    final f = Femme.of(context);
    if (_view?.pane == id) return;
    _view?.removeListener(_onView);
    _view?.dispose();
    _bells = 0;
    _view = TermView.forLink(f.link, id)
      ..addListener(_onView)
      ..start();
  }

  void _onView() {
    final v = _view;
    if (v == null || !mounted) return;
    if (v.bells > _bells) {
      _bells = v.bells;
      HapticFeedback.mediumImpact();
    }
    setState(() {});
  }

  void _send(TermKey k) => _view?.key(k.code, mods: k.mods);

  Future<void> _extra(ExtraKey k) async {
    switch (k) {
      case ExtraKey(:final mod?):
        setState(() => _mods.tap(mod));
      case ExtraKey.paste:
        final data = await Clipboard.getData(Clipboard.kTextPlain);
        final t = data?.text;
        if (t != null && t.isNotEmpty) _view?.paste(t);
      default:
        _send((code: k.code!, mods: _mods.take()));
        setState(() {});
    }
  }

  Future<void> _newPane() async {
    if (_opening) return;
    setState(() => _opening = true);
    final f = Femme.of(context);
    try {
      final sid = _picked?.sid ?? widget.sid;
      final r = await f.link.req(Req.paneOpen(sid));
      if (mounted) setState(() => _picked = r.pane);
    } catch (e) {
      _flash('could not open a shell: $e');
    } finally {
      if (mounted) setState(() => _opening = false);
    }
  }

  Future<void> _close(PaneId id) async {
    final f = Femme.of(context);
    final req = closeReq(f.store, id);
    final nav = Navigator.of(context);
    if (req != null) {
      try {
        await f.link.req(req);
      } catch (e) {
        _flash('could not close: $e');
        return;
      }
    }
    if (!mounted) return;
    final rest = terminalTabs(f.store).where((p) => p.id != id).toList();
    if (rest.isEmpty) {
      nav.maybePop();
    } else {
      setState(() => _picked = rest[_ix.clamp(0, rest.length - 1)].id);
    }
  }

  void _flash(String text) {
    if (!mounted) return;
    setState(() => _toast = text);
    Future.delayed(const Duration(seconds: 2), () {
      if (mounted && _toast == text) setState(() => _toast = null);
    });
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final tabs = terminalTabs(f.store);
    // Opened on a session: its first pane, until one is picked.
    _picked ??= terminalPanes(f.store, widget.sid).firstOrNull?.id;
    final picked = _picked;
    PaneId? shown;
    final cur = _view;
    if (cur != null && cur.pane == picked && cur.exited != null) {
      // An ended pane stays on show with its card until the user leaves it.
      shown = picked;
    } else if (picked != null && !sessionEnded(f.store, picked.sid)) {
      final at = onShow(tabs, picked, _ix);
      if (at != null) {
        _ix = at;
        shown = tabs[at].id;
      }
    }
    if (shown != null) _attach(shown);
    final view = shown == null ? null : _view;
    final title = shown == null
        ? 'Terminal'
        : paneLabel(f.store, shown, view?.grid.title);
    final muted = FemmeColors.mutedC.resolveFrom(context);

    return CupertinoPageScaffold(
      navigationBar: CupertinoNavigationBar(
        middle: Text(title, overflow: TextOverflow.ellipsis),
        trailing: CupertinoButton(
          padding: EdgeInsets.zero,
          onPressed: _opening ? null : _newPane,
          child: _opening
              ? const CupertinoActivityIndicator()
              : const Icon(CupertinoIcons.add),
        ),
      ),
      child: SafeArea(
        bottom: false,
        child: Column(
          children: [
            if (tabs.length > 1)
              _PaneTabs(
                panes: tabs,
                selected: tabs.any((p) => p.id == shown) ? shown : null,
                label: (p) => paneLabel(
                  f.store,
                  p.id,
                  p.id == view?.pane ? view?.grid.title : null,
                ),
                onSelect: (id) => setState(() => _picked = id),
              ),
            Expanded(
              child: view == null
                  ? sessionEnded(f.store, picked?.sid ?? widget.sid)
                        ? _ClosedCard(
                            title: 'session closed',
                            onBack: () => Navigator.of(context).maybePop(),
                          )
                        : _Empty(opening: _opening, onOpen: _newPane)
                  : Stack(
                      children: [
                        Positioned.fill(
                          child: TermSurface(
                            view: view,
                            onTap: () {
                              _focus.hasFocus
                                  ? SystemChannels.textInput.invokeMethod(
                                      'TextInput.show',
                                    )
                                  : _focus.requestFocus();
                            },
                            onCopied: (n) =>
                                _flash('copied $n line${n == 1 ? '' : 's'}'),
                            mods: _mods,
                            onModsUsed: () => setState(() {}),
                          ),
                        ),
                        Positioned(
                          left: 0,
                          top: 0,
                          child: TermInput(
                            focus: _focus,
                            mods: _mods,
                            onKey: (k) {
                              _send(k);
                              setState(() {});
                            },
                            onPaste: (t) => view.paste(t),
                          ),
                        ),
                        if (!view.live && view.exited == null)
                          Positioned(
                            top: 8,
                            left: 0,
                            right: 0,
                            child: Center(
                              child: Text(
                                f.link.isOnline
                                    ? 'attaching…'
                                    : 'offline, reconnecting…',
                                style: TextStyle(color: muted, fontSize: 13),
                              ),
                            ),
                          ),
                        if (view.exited != null)
                          Positioned.fill(
                            child: _ClosedCard(
                              title: endedTitle(f.store, view.pane),
                              status: view.exited,
                              canClose:
                                  !sessionEnded(f.store, view.pane.sid) &&
                                  closeReq(f.store, view.pane) != null,
                              onClose: () => _close(view.pane),
                              onBack: () => Navigator.of(context).maybePop(),
                            ),
                          ),
                        if (_toast != null)
                          Positioned(
                            bottom: 12,
                            left: 0,
                            right: 0,
                            child: Center(child: _Toast(_toast!)),
                          ),
                      ],
                    ),
            ),
            if (view != null && view.exited == null)
              ExtraKeysRow(mods: _mods, onKey: _extra),
            // Above the home indicator when the keyboard is down.
            SizedBox(
              height: MediaQuery.viewInsetsOf(context).bottom > 0
                  ? 0
                  : MediaQuery.paddingOf(context).bottom,
            ),
          ],
        ),
      ),
    );
  }
}

class _PaneTabs extends StatelessWidget {
  const _PaneTabs({
    required this.panes,
    required this.selected,
    required this.label,
    required this.onSelect,
  });
  final List<Pane> panes;
  final PaneId? selected;
  final String Function(Pane) label;
  final void Function(PaneId id) onSelect;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.fromLTRB(8, 6, 8, 6),
    child: SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: CupertinoSlidingSegmentedControl<PaneId>(
        groupValue: selected,
        onValueChanged: (id) {
          if (id == null) return;
          HapticFeedback.selectionClick();
          onSelect(id);
        },
        children: {
          for (final p in panes)
            p.id: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              child: Text(
                label(p),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(fontSize: 13),
              ),
            ),
        },
      ),
    ),
  );
}

class _Empty extends StatelessWidget {
  const _Empty({required this.opening, required this.onOpen});
  final bool opening;
  final VoidCallback onOpen;

  @override
  Widget build(BuildContext context) => Center(
    child: Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          'No terminal in this session.',
          style: TextStyle(color: FemmeColors.mutedC.resolveFrom(context)),
        ),
        const SizedBox(height: 12),
        CupertinoButton.filled(
          onPressed: opening ? null : onOpen,
          child: const Text('Open a shell'),
        ),
      ],
    ),
  );
}

/// The card over an ended terminal, or in place of one whose session is gone: the desktop's
/// `session closed` card (`frame.rs` `Card::Closed`), with the program's exit status under it.
class _ClosedCard extends StatelessWidget {
  const _ClosedCard({
    required this.title,
    this.status,
    this.canClose = false,
    this.onClose,
    required this.onBack,
  });
  final String title;
  final int? status;
  final bool canClose;
  final VoidCallback? onClose;
  final VoidCallback onBack;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return ColoredBox(
      color: FemmeColors.paperC.resolveFrom(context).withValues(alpha: 0.7),
      child: Center(
        child: Container(
          margin: const EdgeInsets.all(24),
          padding: const EdgeInsets.all(20),
          decoration: BoxDecoration(
            color: FemmeColors.paperC.resolveFrom(context),
            border: Border.all(color: ink, width: 1.2),
            boxShadow: [BoxShadow(color: ink, offset: const Offset(3, 3))],
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(title, style: TextStyle(color: ink, fontSize: 17)),
              const SizedBox(height: 4),
              Text(
                status == null
                    ? 'pick another one in the list'
                    : 'process exited ($status)',
                style: TextStyle(color: muted, fontSize: 13),
              ),
              const SizedBox(height: 16),
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  CupertinoButton(onPressed: onBack, child: const Text('Back')),
                  if (canClose)
                    CupertinoButton.filled(
                      onPressed: onClose,
                      child: const Text('Close'),
                    ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _Toast extends StatelessWidget {
  const _Toast(this.text);
  final String text;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
      decoration: BoxDecoration(
        color: FemmeColors.paperC.resolveFrom(context),
        border: Border.all(color: ink, width: 1.2),
        boxShadow: [BoxShadow(color: ink, offset: const Offset(2, 2))],
      ),
      child: Text(text, style: TextStyle(color: ink, fontSize: 14)),
    );
  }
}
