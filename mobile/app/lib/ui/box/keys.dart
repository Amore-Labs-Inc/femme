import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/rpc.dart';
import '../../state/box.dart';
import '../../state/box_flows.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';
import 'github.dart';
import 'poll.dart';

/// The board's API keys: list, add (the secret shown once), connect GitHub, revoke.
class KeysSection extends StatefulWidget {
  const KeysSection({super.key});

  @override
  State<KeysSection> createState() => _KeysSectionState();
}

class _KeysSectionState extends State<KeysSection> with Polling {
  List<ApiKey>? _keys;
  String? _error;

  /// Each key's GitHub side (a sign-in in progress shows its code), by name.
  Map<String, KeyGithub> _github = const {};

  @override
  Duration get every => const Duration(minutes: 1);

  Future<Resp> _req(Object r) => Femme.of(context).link.req(r);

  @override
  Future<void> poll() async {
    try {
      final body = await boardCall(_req, 'GET', '/api/keys');
      if (mounted) {
        setState(() {
          _keys = parseKeys(body);
          _github = parseKeysGithub(body);
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = describeError(e));
    }
  }

  Future<void> _add() async {
    final made = await showCupertinoSheet<(String, String)>(
      context: context,
      scrollableBuilder: (_, _) => const _NewKeySheet(),
    );
    if (made == null || !mounted) return;
    try {
      final body = await boardCall(
        _req,
        'POST',
        '/api/keys',
        body: {'name': made.$1, 'github_token': made.$2},
      );
      actionHaptic();
      final secret = parseNewKeySecret(body);
      await poll();
      if (secret != null && mounted) await _showSecret(made.$1, secret);
    } catch (e) {
      if (mounted) await showProblem(context, 'Key not made', e);
    }
  }

  Future<void> _showSecret(String name, String secret) =>
      showCupertinoDialog<void>(
        context: context,
        builder: (c) => CupertinoAlertDialog(
          title: Text('Key $name'),
          content: Column(
            children: [
              const Text('Copy it now: the box never shows it again.'),
              const SizedBox(height: 8),
              Text(
                secret,
                style: const TextStyle(
                  fontFamily: monoFamily,
                  fontFamilyFallback: monoFallback,
                  fontSize: 13,
                ),
              ),
            ],
          ),
          actions: [
            CupertinoDialogAction(
              onPressed: () {
                Clipboard.setData(ClipboardData(text: secret));
                tapHaptic();
              },
              child: const Text('Copy'),
            ),
            CupertinoDialogAction(
              isDefaultAction: true,
              onPressed: () => Navigator.pop(c),
              child: const Text('Done'),
            ),
          ],
        ),
      );

  /// A key's actions: connect GitHub (or go back to its code), revoke.
  Future<void> _actions(ApiKey k) async {
    final waiting = _github[k.name]?.device != null;
    final pick = await showCupertinoModalPopup<String>(
      context: context,
      builder: (c) => CupertinoActionSheet(
        title: Text(k.name),
        message: Text('pushes as ${k.account}'),
        actions: [
          CupertinoActionSheetAction(
            onPressed: () => Navigator.pop(c, 'github'),
            child: Text(waiting ? 'GitHub code' : 'Connect GitHub'),
          ),
          CupertinoActionSheetAction(
            isDestructiveAction: true,
            onPressed: () => Navigator.pop(c, 'revoke'),
            child: const Text('Revoke'),
          ),
        ],
        cancelButton: CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(c),
          child: const Text('Cancel'),
        ),
      ),
    );
    if (!mounted) return;
    switch (pick) {
      case 'github':
        await _connect(k);
      case 'revoke':
        await _revoke(k);
    }
  }

  /// "connect GitHub": the device-code sheet, then the list as it is now.
  Future<void> _connect(ApiKey k) async {
    final key = _github[k.name] ?? KeyGithub(k.name, k.account, null, k.error);
    await showConnectGithub(context, _req, key);
    if (mounted) await poll();
  }

  Future<void> _revoke(ApiKey k) async {
    final ok = await confirmAction(
      context,
      title: 'Revoke ${k.name}?',
      message: 'Anything using it stops working.',
      action: 'Revoke',
    );
    if (!ok || !mounted) return;
    try {
      final body = await boardCall(
        _req,
        'DELETE',
        '/api/keys/${Uri.encodeComponent(k.name)}',
      );
      actionHaptic();
      if (mounted) setState(() => _keys = parseKeys(body));
      await poll();
    } catch (e) {
      if (mounted) await showProblem(context, 'Not revoked', e);
    }
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final keys = _keys;
    return CupertinoListSection.insetGrouped(
      header: const Text('API keys'),
      children: [
        if (keys == null && _error == null)
          const CupertinoListTile(title: CupertinoActivityIndicator()),
        if (_error != null)
          CupertinoListTile(
            title: Text(_error!, style: TextStyle(color: muted)),
          ),
        for (final k in keys ?? const <ApiKey>[])
          CupertinoListTile(
            leading: const Icon(CupertinoIcons.lock),
            title: Text(k.name),
            onTap: () => _actions(k),
            subtitle: Text(
              [
                '…${k.hint}',
                'pushes as ${k.account}',
                if (k.created != null) '${ago(k.created!)} ago',
                if (_github[k.name]?.device != null) 'GitHub code waiting',
                ?k.error,
              ].join(' · '),
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
            ),
            trailing: CupertinoButton(
              sizeStyle: CupertinoButtonSize.small,
              padding: EdgeInsets.zero,
              onPressed: () => _revoke(k),
              child: const Text(
                'Revoke',
                style: TextStyle(color: CupertinoColors.destructiveRed),
              ),
            ),
          ),
        if (_error == null || keys != null)
          CupertinoListTile(
            leading: const Icon(CupertinoIcons.add_circled),
            title: const Text('Add key'),
            onTap: _add,
          ),
      ],
    );
  }
}

class _NewKeySheet extends StatefulWidget {
  const _NewKeySheet();

  @override
  State<_NewKeySheet> createState() => _NewKeySheetState();
}

class _NewKeySheetState extends State<_NewKeySheet> {
  final _name = TextEditingController();
  final _token = TextEditingController();

  @override
  void dispose() {
    _name.dispose();
    _token.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final ok = validKeyName(_name.text.trim());
    return SheetPage(
      title: 'New API key',
      doneLabel: 'Add',
      onDone: ok
          ? () =>
                Navigator.of(context)
                    .pop((_name.text.trim(), _token.text.trim()))
          : null,
      child: ListView(
        children: [
          CupertinoListSection.insetGrouped(
            footer: const Text(
              'Name: lowercase letters, digits and -, up to 32. The GitHub token is optional: '
              'without one its clones push as the box.',
            ),
            children: [
              Padding(
                padding: const EdgeInsets.all(10),
                child: CupertinoTextField(
                  controller: _name,
                  autofocus: true,
                  autocorrect: false,
                  placeholder: 'name',
                  onChanged: (_) => setState(() {}),
                ),
              ),
              Padding(
                padding: const EdgeInsets.all(10),
                child: CupertinoTextField(
                  controller: _token,
                  autocorrect: false,
                  obscureText: true,
                  placeholder: 'GitHub token (optional)',
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }
}
