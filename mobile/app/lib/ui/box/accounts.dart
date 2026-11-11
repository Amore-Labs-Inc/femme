import 'dart:async';

import 'package:flutter/cupertino.dart';

import '../../net/link.dart';
import '../../net/rpc.dart';
import '../../state/box.dart';
import '../../state/box_flows.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';
import 'add_account.dart';
import 'poll.dart';
import 'prs.dart';

/// The box's Claude accounts (cswap): usage bars, sign-in warnings, switch, rotation caps.
class AccountsSection extends StatefulWidget {
  const AccountsSection({super.key});

  @override
  State<AccountsSection> createState() => _AccountsSectionState();
}

class _AccountsSectionState extends State<AccountsSection> with Polling {
  List<Account>? _accounts;
  String? _error;
  Map<int, AccountCap> _caps = {};
  bool _capsKnown = true;
  int? _defaultCap;
  int? _switching;

  /// What `/api/meta` says: the default cap and the scheduler's word (the footer).
  BoxMeta? _meta;

  /// Who the box's AWS CLI runs as; null until the first read.
  AwsIdentity? _aws;

  /// The link of a Claude sign-in in progress on the box.
  String? _loginLink;

  /// Caps typed but not sent yet (sent 500 ms after the last change).
  final _pending = <int, (int?, int?)>{};
  Timer? _send;

  @override
  Duration get every => const Duration(seconds: 10);

  /// The link caps are sent on, kept here so the flush in [dispose] needs no context lookup
  /// (`Femme.of` on a deactivated element asserts).
  late Link _link;

  @override
  void didChangeDependencies() {
    _link = Femme.of(context).link;
    super.didChangeDependencies();
  }

  @override
  void dispose() {
    _send?.cancel();
    _flush();
    super.dispose();
  }

  @override
  Future<void> poll() async {
    final link = Femme.of(context).link;
    Future<Resp> req(Object r) => link.req(r);
    try {
      final body = await boardCall(req, 'GET', '/api/accounts');
      final parsed = parseAccounts(body);
      if (mounted) {
        setState(() {
          _accounts = parsed.accounts ?? _accounts;
          _error = parsed.error;
          _loginLink = parseLoginLink(body);
          _aws = parseAws(body);
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = describeError(e));
    }
    try {
      final caps = (await link.req(Req.caps)).caps;
      if (mounted) setState(() => _caps = {for (final c in caps) c.account: c});
    } catch (e) {
      if (mounted) setState(() => _capsKnown = !isUnsupported(e));
    }
    try {
      final meta = parseMeta(await boardCall(req, 'GET', '/api/meta'));
      if (mounted) {
        setState(() {
          _meta = meta;
          _defaultCap = meta.cap ?? _defaultCap;
        });
      }
    } catch (_) {
      // the footer says less without it; the cap placeholder just says "default"
    }
  }

  /// "+ add": a Claude sign-in, or the one already in progress.
  Future<void> _add() async {
    final link = Femme.of(context).link;
    final added = await showAddAccount(
      context,
      (r) => link.req(r),
      link: _loginLink,
    );
    if (mounted) await poll();
    if (added && mounted) actionHaptic();
  }

  Future<void> _switch(Account a) async {
    final link = Femme.of(context).link;
    actionHaptic();
    setState(() => _switching = a.number);
    try {
      await boardCall(
        (r) => link.req(r),
        'POST',
        '/api/accounts/switch',
        body: {'account': '${a.number}'},
      );
      await poll();
    } catch (e) {
      if (mounted) await showProblem(context, 'Switch failed', e);
    } finally {
      if (mounted) setState(() => _switching = null);
    }
  }

  (int?, int?) _capOf(int account) {
    final p = _pending[account];
    if (p != null) return p;
    final c = _caps[account];
    return (c?.fiveHour, c?.weekly);
  }

  /// The stepper has already clicked ([CapStepper] haptics each step).
  void _setCap(int account, (int?, int?) v) {
    setState(() => _pending[account] = v);
    _send?.cancel();
    _send = Timer(const Duration(milliseconds: 500), _flush);
  }

  void _flush() {
    if (_pending.isEmpty) return;
    final link = _link;
    final todo = Map.of(_pending);
    _pending.clear();
    for (final e in todo.entries) {
      _caps[e.key] = AccountCap(e.key, e.value.$1, e.value.$2);
      link
          .req(Req.setCap(e.key, fiveHour: e.value.$1, weekly: e.value.$2))
          .catchError((Object err) {
            if (mounted) showProblem(context, 'Cap not saved', err);
            return const Resp('Ok', null);
          });
    }
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final accounts = _accounts;
    final now = DateTime.now();
    return CupertinoListSection.insetGrouped(
      header: const Text('Accounts'),
      footer: _meta == null && !_capsKnown
          ? null
          : sectionFooter(
              context,
              [
                if (_meta != null) '${schedulerLine(_meta!)}.',
                if (_capsKnown)
                  'Caps: the box moves on when an account reaches them '
                      '($capMin–$capMax %, empty = box default'
                      '${_defaultCap == null ? '' : ' $_defaultCap %'}).',
              ].join('\n'),
            ),
      children: [
        if (accounts == null && _error == null)
          const CupertinoListTile(title: CupertinoActivityIndicator()),
        if (_error != null)
          CupertinoListTile(
            title: Text(_error!, style: TextStyle(color: muted)),
          ),
        for (final a in accounts ?? const <Account>[])
          _AccountTile(
            account: a,
            now: now,
            switching: _switching == a.number,
            onSwitch: a.active || _switching != null ? null : () => _switch(a),
            caps: _capsKnown ? _capOf(a.number) : null,
            defaultCap: _defaultCap,
            onCap: (v) => _setCap(a.number, v),
          ),
        if (accounts != null)
          CupertinoListTile(
            leading: const Icon(CupertinoIcons.add_circled),
            title: Text(
              _loginLink == null
                  ? 'Add Claude account'
                  : 'Finish adding the account',
            ),
            onTap: _switching == null ? _add : null,
          ),
        if (_aws != null) _AwsTile(_aws!),
      ],
    );
  }
}

/// AWS: who the box's CLI runs as, with the green bar when it has an identity (desktop
/// `accounts/view.rs` `aws_card`).
class _AwsTile extends StatelessWidget {
  const _AwsTile(this.aws);
  final AwsIdentity aws;

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final small = TextStyle(color: muted, fontSize: 13);
    return IntrinsicHeight(
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Container(width: 4, color: aws.active ? FemmeColors.work : null),
          Expanded(
            child: Padding(
              padding: const EdgeInsets.fromLTRB(12, 10, 16, 10),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const Text(
                    'AWS',
                    style: TextStyle(fontWeight: FontWeight.w600),
                  ),
                  Text(aws.line, style: small),
                  if (!aws.active && aws.detail != null)
                    Text(aws.detail!, style: small),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _AccountTile extends StatelessWidget {
  const _AccountTile({
    required this.account,
    required this.now,
    required this.switching,
    required this.onSwitch,
    required this.caps,
    required this.defaultCap,
    required this.onCap,
  });
  final Account account;
  final DateTime now;
  final bool switching;
  final VoidCallback? onSwitch;
  final (int?, int?)? caps;
  final int? defaultCap;
  final void Function((int?, int?)) onCap;

  @override
  Widget build(BuildContext context) {
    final a = account;
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final sign = a.signIn(now);
    final c = caps;
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 10, 16, 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  a.title,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
              ),
              if (a.active) const StatusChip('in use', FemmeColors.work),
              if (a.disabled)
                const StatusChip('disabled', FemmeColors.done, faint: true),
              if (!a.active)
                switching
                    ? const CupertinoActivityIndicator()
                    : CupertinoButton(
                        sizeStyle: CupertinoButtonSize.small,
                        padding: const EdgeInsets.only(left: 8),
                        onPressed: onSwitch,
                        child: const Text('Switch'),
                      ),
            ],
          ),
          if (a.fiveHour != null) _Bar('5 h', a.fiveHour!, now),
          if (a.sevenDay != null) _Bar('7 d', a.sevenDay!, now),
          for (final s in a.scoped) _Bar(s.name!.toLowerCase(), s, now),
          if (a.note(now) case final note? when note.isNotEmpty)
            Padding(
              padding: const EdgeInsets.only(top: 4),
              child: Text(note, style: TextStyle(color: muted, fontSize: 13)),
            ),
          if (sign != null)
            Padding(
              padding: const EdgeInsets.only(top: 4),
              child: Text(
                sign.$1,
                style: TextStyle(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                  color: sign.$2 == Level.bad
                      ? FemmeColors.bad
                      : FemmeColors.pause,
                ),
              ),
            ),
          if (c != null)
            Padding(
              padding: const EdgeInsets.only(top: 6),
              child: Row(
                children: [
                  Expanded(
                    child: CapStepper(
                      label: '5 h cap',
                      value: c.$1,
                      fallback: defaultCap,
                      onChanged: (v) => onCap((v, c.$2)),
                    ),
                  ),
                  const SizedBox(width: 8),
                  Expanded(
                    child: CapStepper(
                      label: '7 d cap',
                      value: c.$2,
                      fallback: defaultCap,
                      onChanged: (v) => onCap((c.$1, v)),
                    ),
                  ),
                ],
              ),
            ),
        ],
      ),
    );
  }
}

class _Bar extends StatelessWidget {
  const _Bar(this.label, this.usage, this.now);
  final String label;
  final Usage usage;
  final DateTime now;

  @override
  Widget build(BuildContext context) {
    final fill = usage.fill;
    final color = fill >= .9
        ? FemmeColors.bad
        : fill >= .7
        ? FemmeColors.pause
        : FemmeColors.work;
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return Padding(
      padding: const EdgeInsets.only(top: 6),
      child: Row(
        children: [
          SizedBox(
            width: 44,
            child: Text(label, style: TextStyle(fontSize: 13, color: muted)),
          ),
          Expanded(
            child: UsageTrack(fill: fill, color: color),
          ),
          const SizedBox(width: 8),
          Text(usage.text(now), style: TextStyle(fontSize: 13, color: muted)),
        ],
      ),
    );
  }
}

/// A plain rounded progress track: system fill under, [color] over the first [fill] of it.
class UsageTrack extends StatelessWidget {
  const UsageTrack({super.key, required this.fill, required this.color});
  final double fill;
  final Color color;

  static const radius = BorderRadius.all(Radius.circular(4));

  @override
  Widget build(BuildContext context) => ClipRRect(
    borderRadius: radius,
    child: Container(
      height: 8,
      color: CupertinoColors.systemFill.resolveFrom(context),
      alignment: Alignment.centerLeft,
      child: FractionallySizedBox(
        widthFactor: fill.clamp(0, 1).toDouble(),
        heightFactor: 1,
        child: DecoratedBox(
          decoration: BoxDecoration(color: color, borderRadius: radius),
        ),
      ),
    ),
  );
}

/// A 50–99 % stepper; empty (null) means the box default, shown greyed. Each step clicks; holding
/// − or + repeats after [repeatDelay], one step every [repeatEvery], so 80 → 97 is one hold.
class CapStepper extends StatefulWidget {
  const CapStepper({
    super.key,
    required this.label,
    required this.value,
    required this.onChanged,
    this.fallback,
  });
  final String label;
  final int? value;
  final int? fallback;
  final void Function(int?) onChanged;

  static const repeatDelay = Duration(milliseconds: 400);
  static const repeatEvery = Duration(milliseconds: 80);

  @override
  State<CapStepper> createState() => _CapStepperState();
}

class _CapStepperState extends State<CapStepper> {
  Timer? _hold;

  @override
  void dispose() {
    _hold?.cancel();
    super.dispose();
  }

  /// The value one step of [dir] (−1 / +1) moves to from the current one, null at the end of
  /// the range. The first step from "default" lands on the default itself.
  int? _next(int dir) {
    final v = widget.value;
    final base = v ?? widget.fallback ?? 80;
    if (dir < 0 ? base <= capMin : base >= capMax) return null;
    return v == null ? base : base + dir;
  }

  void _step(int dir) {
    final n = _next(dir);
    if (n == null) {
      _stop();
      return;
    }
    tapHaptic();
    widget.onChanged(n);
  }

  void _start(int dir) {
    _step(dir);
    _hold?.cancel();
    _hold = Timer(CapStepper.repeatDelay, () {
      _hold = Timer.periodic(CapStepper.repeatEvery, (_) {
        if (mounted) _step(dir);
      });
    });
  }

  void _stop() {
    _hold?.cancel();
    _hold = null;
  }

  /// A 44 × 44 target around a small glyph; holding repeats.
  Widget _button(IconData icon, int dir, String what) {
    final enabled = _next(dir) != null;
    final color = enabled
        ? CupertinoTheme.of(context).primaryColor
        : CupertinoColors.tertiaryLabel.resolveFrom(context);
    return Semantics(
      button: true,
      enabled: enabled,
      label: '$what ${widget.label}',
      onTap: enabled ? () => _step(dir) : null,
      excludeSemantics: true,
      child: GestureDetector(
        behavior: HitTestBehavior.opaque,
        onTapDown: enabled ? (_) => _start(dir) : null,
        onTapUp: (_) => _stop(),
        onTapCancel: _stop,
        child: SizedBox.square(
          dimension: 44,
          child: Icon(icon, size: 22, color: color),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final v = widget.value;
    return DecoratedBox(
      decoration: BoxDecoration(
        color: CupertinoColors.tertiarySystemFill.resolveFrom(context),
        borderRadius: const BorderRadius.all(Radius.circular(10)),
      ),
      child: Row(
        children: [
          _button(CupertinoIcons.minus_circle, -1, 'lower'),
          Expanded(
            child: GestureDetector(
              onLongPress: v == null ? null : () => widget.onChanged(null),
              child: Column(
                children: [
                  Text(
                    widget.label,
                    style: TextStyle(fontSize: 11, color: muted),
                  ),
                  FittedBox(
                    fit: BoxFit.scaleDown,
                    child: Text(
                      v == null ? 'default' : '$v%',
                      maxLines: 1,
                      style: TextStyle(
                        fontWeight: FontWeight.w600,
                        color: v == null ? muted : null,
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
          _button(CupertinoIcons.plus_circle, 1, 'raise'),
          if (v != null)
            Semantics(
              button: true,
              label: 'clear ${widget.label}',
              excludeSemantics: true,
              child: CupertinoButton(
                padding: EdgeInsets.zero,
                minimumSize: const Size(44, 44),
                onPressed: () {
                  tapHaptic();
                  widget.onChanged(null);
                },
                child: Icon(
                  CupertinoIcons.clear_circled_solid,
                  size: 18,
                  color: muted,
                ),
              ),
            ),
        ],
      ),
    );
  }
}
